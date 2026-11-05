//! `tock-transport-openocd`: drives a long-lived openocd over its TCL RPC port.
//!
//! Nothing here parses openocd's prose. Startup picks the port itself, so
//! readiness is a successful TCP connect and failure is openocd exiting --
//! two unambiguous events rather than a race against stderr. Each TCL command
//! comes back as a status plus whatever openocd logged while running it, so a
//! failure reaches the user in openocd's own words instead of as a number.
//!
//! Options:
//! - `config`: openocd config files, a string or a list. An entry that names a
//!   file next to the config that declared it is resolved to that file;
//!   anything else is passed through for openocd's own script search path, so
//!   `board/nordic_nrf52_dk.cfg` keeps working.
//! - `command`: the openocd executable (default `openocd`).
//! - `debug_level`: openocd's log verbosity (default 2).
//!
//! The target is halted for the life of the session: flash operations need it,
//! and leaving it halted keeps every request working the same way.

use std::io::{BufRead as _, BufReader, Read as _, Seek as _, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::NamedTempFile;
use tock_proto::{
    Capability, Connection, ErrorKind, FlashInfo, Options, ProtoError, Request, Response,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const STARTUP_POLL: Duration = Duration::from_millis(10);

/// Separates the fields of a TCL reply. A control byte, so it cannot collide
/// with a path or an openocd message.
const FIELD: char = '\u{1f}';

/// Either kind of failure, so one `?` chain can carry both.
///
/// The distinction is whether there is still an answer to give. An operation
/// that failed has one, and the client may send more; a connection that failed
/// has none, and nothing further can be sent. Kept here rather than shared:
/// `file.rs` has no second kind, and a helper designed against one consumer is
/// a guess.
enum Failure {
    /// Answer the client with this. The connection is fine.
    Operation(Box<Response>),
    /// There is no answer to give.
    Connection(ProtoError),
}

impl From<Response> for Failure {
    fn from(r: Response) -> Failure {
        Failure::Operation(Box::new(r))
    }
}

impl From<ProtoError> for Failure {
    fn from(e: ProtoError) -> Failure {
        Failure::Connection(e)
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Failure {
        Failure::Connection(e.into())
    }
}

/// What one TCL command did. `rc` is openocd's own error code, not TCL's 0/1:
/// anything non-zero failed.
#[derive(Debug, PartialEq, Eq)]
struct TclReply {
    rc: i64,
    msg: String,
    log: String,
}

impl TclReply {
    /// Split a reply of the form `rc FIELD msg FIELD log`.
    ///
    /// `rc` is not always a bare integer: a TCL-level error prefixes it with
    /// openocd's log line, so the code is the last whitespace-separated token.
    fn parse(raw: &str) -> Result<TclReply, ProtoError> {
        let unreadable = || ProtoError::failed(format!("unreadable TCL reply: {raw:?}"));
        let mut fields = raw.splitn(3, FIELD);
        let (Some(status), Some(msg), Some(log)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(unreadable());
        };
        let rc = status
            .split_whitespace()
            .next_back()
            .and_then(|token| token.parse::<i64>().ok())
            .ok_or_else(unreadable)?;
        Ok(TclReply {
            rc,
            msg: msg.to_string(),
            log: log.to_string(),
        })
    }

    fn failed(&self) -> bool {
        self.rc != 0
    }

    /// The best text available. openocd commands leave their explanation in
    /// the log and `msg` empty; TCL-level errors do the opposite. When a
    /// command logs nothing, `capture` falls through to the script's own
    /// result -- the rc rendered as a string -- which says nothing worth
    /// repeating.
    fn message(&self) -> String {
        let msg = self.msg.trim();
        if !msg.is_empty() {
            return msg.to_string();
        }
        let log = self.log.trim();
        if !log.is_empty() && log != self.rc.to_string() {
            return log.to_string();
        }
        format!("openocd returned {}", self.rc)
    }
}

/// The erase unit, checked. A value that cannot be an erase unit is worse
/// than a missing one: zero divides by zero in [`sector_span`], and a
/// non-power-of-two straddles the device's real sectors, so `erase` destroys
/// bytes the core never sent.
fn page_size_from(configured: Option<u64>) -> Result<u64, Response> {
    let missing = || {
        Response::error(
            ErrorKind::Selection,
            "the openocd transport needs the flash erase unit",
        )
        .with_suggestion(
            "add --opt page_size=4096\n\
             or give the board file a [connection.openocd] page_size entry",
        )
    };
    match configured {
        None => Err(missing()),
        Some(v) if v == 0 || !v.is_power_of_two() => Err(Response::error(
            ErrorKind::Selection,
            format!("page_size {v} cannot be a flash erase unit: it must be a power of two"),
        )
        .with_suggestion("check [connection.openocd] page_size, e.g. page_size = 4096")),
        Some(v) => Ok(v),
    }
}

/// The sectors a write touches, as `(address, len)`, or `None` when it writes
/// nothing. Everything inside this span is erased by `flash write_image
/// erase`, so everything inside it has to be rewritten.
fn sector_span(page_size: u64, address: u64, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let start = address - address % page_size;
    let end = (address + len).div_ceil(page_size) * page_size;
    Some((start, end - start))
}

/// How a write has to be carried out.
#[derive(Debug, PartialEq, Eq)]
enum WritePlan {
    /// Already whole sectors: send it as it stands.
    Direct,
    /// Read this span back, overlay, and write the whole thing, so `erase`
    /// does not take the rest of the sector with it.
    Merge { start: u64, len: u64 },
}

/// Decide how to write `len` bytes at `address`, or `None` when there is
/// nothing to write. Every write goes through here, so an empty one cannot
/// reach openocd just because its address happened to be sector-aligned.
fn write_plan(page_size: u64, address: u64, len: u64) -> Option<WritePlan> {
    let (start, span) = sector_span(page_size, address, len)?;
    if address.is_multiple_of(page_size) && len.is_multiple_of(page_size) {
        return Some(WritePlan::Direct);
    }
    Some(WritePlan::Merge { start, len: span })
}

/// Resolve one `config` entry. Returns the entry unchanged unless it names a
/// file next to the config that declared it.
fn resolve_config_entry(entry: &str, base: Option<&Path>) -> String {
    let path = Path::new(entry);
    if path.is_absolute() {
        return entry.to_string();
    }
    if let Some(base) = base {
        let beside = base.join(path);
        if beside.is_file() {
            return beside.display().to_string();
        }
    }
    entry.to_string()
}

/// The `config` option as a list, accepting either a bare string or an array.
fn resolve_configs(options: &Options<'_>) -> Vec<String> {
    let Some(value) = options.get("config") else {
        return Vec::new();
    };
    let base = options.base_dir("config");
    let entries: Vec<&str> = match value {
        serde_json::Value::String(s) => vec![s.as_str()],
        serde_json::Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
        _ => Vec::new(),
    };
    entries
        .into_iter()
        .map(|e| resolve_config_entry(e, base.as_deref()))
        .collect()
}

/// A running openocd and the connection to its TCL port.
struct Session {
    openocd: Child,
    tcl: BufReader<TcpStream>,
    /// openocd's stderr, which would otherwise fill its pipe buffer and block
    /// the process. Kept so a failure can quote it.
    log: NamedTempFile,
    /// The erase unit, from the `page_size` option. openocd erases whole
    /// sectors, so this is what a write has to be rounded out to.
    page_size: u64,
}

impl Session {
    fn start(
        command: &str,
        configs: &[String],
        debug_level: i64,
        page_size: u64,
    ) -> Result<Session, ProtoError> {
        // Choosing the port here means there is nothing to discover later.
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = probe.local_addr()?.port();
        drop(probe);

        let log = NamedTempFile::new()?;
        let mut openocd = Command::new(command)
            .args(configs.iter().flat_map(|c| ["-f", c]))
            .args(["-c", &format!("tcl_port {port}")])
            .args(["-c", "telnet_port disabled"])
            .args(["-c", "gdb_port disabled"])
            .args(["-c", &format!("debug_level {debug_level}")])
            // `init` here rather than over TCL: if it fails, openocd exits and
            // the loop below reports that with its log, which is the clearest
            // error we can give for "the probe or target is not there".
            .args(["-c", "init"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log.reopen()?)
            .spawn()
            .map_err(|e| {
                ProtoError::failed(format!("cannot start `{command}`: {e}")).with_suggestion(
                    "install openocd, or specify its path with --opt command=/path/to/openocd",
                )
            })?;

        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let tcl = loop {
            if let Ok(stream) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
                break BufReader::new(stream);
            }
            match openocd.try_wait()? {
                Some(status) => {
                    let detail = tail_of(&log);
                    return Err(ProtoError::failed(format!(
                        "openocd exited ({status}) before accepting a connection:\n{detail}"
                    )));
                }
                None if Instant::now() >= deadline => {
                    let _ = openocd.kill();
                    let _ = openocd.wait();
                    let detail = tail_of(&log);
                    return Err(ProtoError::failed(format!(
                        "openocd did not open its TCL port within {}s:\n{detail}",
                        STARTUP_TIMEOUT.as_secs()
                    )));
                }
                None => std::thread::sleep(STARTUP_POLL),
            }
        };

        let mut session = Session {
            openocd,
            tcl,
            log,
            page_size,
        };
        // Flash operations need a halted target, and holding it halted for the
        // session keeps every later request working the same way.
        // No usable session without a halted target, so this is fatal too.
        session.run("halt").map_err(|e| match e {
            Failure::Connection(e) => e,
            Failure::Operation(r) => ProtoError::failed(match *r {
                Response::Error { message, .. } => message,
                other => format!("{other:?}"),
            }),
        })?;
        Ok(session)
    }

    /// Send one command and return its reply, whether or not it succeeded.
    fn tcl_cmd(&mut self, cmd: &str) -> Result<TclReply, ProtoError> {
        // `capture` outside `catch` keeps the status while collecting whatever
        // the command logged; inside, it would swallow the status.
        let wrapped = format!(
            r#"set log [capture {{set rc [catch {{{cmd}}} msg]}}]; return "$rc{FIELD}$msg{FIELD}$log""#
        );
        // openocd dying mid-session shows up here as a broken pipe or a short
        // read; either way its last words are in the log, not in the error.
        let died = |detail: String| {
            ProtoError::failed(format!(
                "openocd stopped responding during `{cmd}`: {detail}"
            ))
        };
        let sock = self.tcl.get_mut();
        sock.write_all(wrapped.as_bytes())
            .and_then(|()| sock.write_all(&[0x1a]))
            .and_then(|()| sock.flush())
            .map_err(|_| died(tail_of(&self.log)))?;

        let mut buf = Vec::new();
        let read = self
            .tcl
            .read_until(0x1a, &mut buf)
            .map_err(|_| died(tail_of(&self.log)))?;
        if read == 0 {
            return Err(died(tail_of(&self.log)));
        }
        buf.pop(); // the terminator
        TclReply::parse(&String::from_utf8_lossy(&buf))
    }

    /// Read a range into memory, via a temp file openocd writes.
    fn dump(&mut self, address: u64, len: u64) -> Result<Vec<u8>, Failure> {
        let scratch = NamedTempFile::new()?;
        let path = scratch.path().display().to_string();
        self.run(&format!("dump_image {path} {address:#x} {len}"))?;
        let mut data = Vec::new();
        // Reopen rather than reusing the handle: openocd wrote this file
        // behind our back, and the fresh descriptor cannot be stale.
        scratch.reopen()?.read_to_end(&mut data)?;
        if data.len() as u64 != len {
            return Err(Response::error(
                ErrorKind::Device,
                format!(
                    "openocd dumped {} bytes of the {len} requested at {address:#x}",
                    data.len()
                ),
            )
            .into());
        }
        Ok(data)
    }

    /// Write bytes, erasing the sectors they touch. Whole sectors only: see
    /// `OpenocdTransport::write`.
    fn program(&mut self, address: u64, data: &[u8]) -> Result<(), Failure> {
        let mut scratch = NamedTempFile::new()?;
        scratch.write_all(data)?;
        scratch.flush()?;
        let path = scratch.path().display().to_string();
        self.run(&format!("flash write_image erase {path} {address:#x} bin"))?;
        Ok(())
    }

    /// Send one command and fail if openocd did.
    fn run(&mut self, cmd: &str) -> Result<TclReply, Failure> {
        let reply = self.tcl_cmd(cmd)?;
        if reply.failed() {
            return Err(Response::error(
                ErrorKind::Device,
                format!("openocd: {}", reply.message()),
            )
            .into());
        }
        Ok(reply)
    }
}

impl Drop for Session {
    /// Without this, a transport that dies before `Close` leaves openocd
    /// holding the debug adapter, and the next run fails with the probe busy.
    fn drop(&mut self) {
        let _ = self.openocd.kill();
        let _ = self.openocd.wait();
    }
}

/// The last 4 KiB of openocd's log
fn tail_of(log: &NamedTempFile) -> String {
    const TAIL: u64 = 4096;
    let Ok(mut file) = log.reopen() else {
        return String::from("(no log)");
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len > TAIL {
        let _ = file.seek(std::io::SeekFrom::Start(len - TAIL));
    }
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    let text = text.trim();
    if text.is_empty() {
        String::from("(openocd logged nothing)")
    } else {
        text.to_string()
    }
}

#[derive(Default)]
pub struct OpenocdTransport {
    session: Option<Session>,
    /// The flash this transport was opened for. Every read and write is
    /// checked against it: openocd will happily program whatever address it
    /// is given, including the kernel or a bootloader.
    flash: Option<FlashInfo>,
}

impl OpenocdTransport {
    fn open(&mut self, flash: FlashInfo, options: Options<'_>) -> Result<(), Failure> {
        // Replace any previous session, so its openocd is killed by Drop.
        self.session = None;
        let configs = resolve_configs(&options);
        if configs.is_empty() {
            return Err(Response::error(
                ErrorKind::Selection,
                "the openocd transport needs at least one openocd config file",
            )
            .with_suggestion(
                "add --opt config=board/nordic_nrf52_dk.cfg\n\
                 or give the board file a [connection.openocd] config = [...] entry",
            )
            .into());
        }
        let command = options
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("openocd");
        let debug_level = options
            .get("debug_level")
            .and_then(|v| v.as_i64())
            .unwrap_or(2);
        // No default: guessing an erase unit is how a write silently takes a
        // neighbour with it.
        let page_size = page_size_from(options.get("page_size").and_then(|v| v.as_u64()))?;
        self.session = Some(Session::start(command, &configs, debug_level, page_size)?);
        self.flash = Some(flash);
        Ok(())
    }

    /// Refuse an access outside the flash we were opened for.
    ///
    /// A transport owns its device's address map, and is the only thing that
    /// can enforce it: it is a separate executable, so anyone who can spawn
    /// it can talk to it with nothing in front.
    fn check_range(&self, address: u64, len: u64) -> Result<(), Failure> {
        let Some(flash) = &self.flash else {
            return Err(Response::error(ErrorKind::InvalidRequest, "not open").into());
        };
        if address < flash.start || address + len > flash.start + flash.size {
            return Err(Response::error(
                ErrorKind::InvalidRequest,
                format!(
                    "{address:#x}+{len} is outside flash ({:#x} +{:#x})",
                    flash.start, flash.size
                ),
            )
            .into());
        }
        Ok(())
    }

    fn session(&mut self) -> Result<&mut Session, Failure> {
        self.session
            .as_mut()
            .ok_or_else(|| Response::error(ErrorKind::InvalidRequest, "not open").into())
    }

    fn read(&mut self, address: u64, len: u64) -> Result<Vec<u8>, Failure> {
        if len == 0 {
            return Ok(Vec::new());
        }
        self.check_range(address, len)?;
        self.session()?.dump(address, len)
    }

    /// The core sends whatever bytes changed; openocd can only erase whole
    /// sectors. So anything that does not land on sector boundaries is read
    /// back, overlaid, and written whole -- otherwise `erase` would take the
    /// neighbours with it.
    fn write(&mut self, address: u64, data: &[u8]) -> Result<(), Failure> {
        self.check_range(address, data.len() as u64)?;
        let session = self.session()?;
        match write_plan(session.page_size, address, data.len() as u64) {
            None => Ok(()),
            Some(WritePlan::Direct) => session.program(address, data),
            Some(WritePlan::Merge { start, len }) => {
                let mut merged = session.dump(start, len)?;
                let at = (address - start) as usize;
                merged[at..at + data.len()].copy_from_slice(data);
                session.program(start, &merged)
            }
        }
    }

    fn dispatch(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), Failure> {
        match request {
            Request::Hello { .. } => Ok((
                Response::Hello {
                    protocol: tock_proto::PROTOCOL_VERSION,
                    transport: "openocd".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    capabilities: vec![
                        Capability::MemoryRead,
                        Capability::MemoryWrite,
                        Capability::Reset,
                    ],
                },
                vec![],
            )),
            Request::Open {
                flash,
                options,
                option_bases,
                ..
            } => {
                self.open(flash, Options::new(&options, &option_bases))?;
                Ok((Response::Ok, vec![]))
            }
            Request::Read { address, len } => Ok((Response::Data, self.read(address, len)?)),
            Request::Write { address } => {
                self.write(address, &payload)?;
                Ok((Response::Ok, vec![]))
            }
            // `reset halt`, not `reset`: a bare reset resumes the target, and
            // every other request here assumes it is halted.
            Request::Reset => {
                self.session()?.run("reset halt")?;
                Ok((Response::Ok, vec![]))
            }
            Request::Close => {
                self.session = None; // Drop kills openocd
                Ok((Response::Ok, vec![]))
            }
            other => Err(Response::error(
                ErrorKind::Unsupported,
                format!("openocd transport does not support {other:?}"),
            )
            .into()),
        }
    }
}

impl Connection for OpenocdTransport {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), tock_proto::ProtoError> {
        match self.dispatch(request, payload) {
            Ok(answer) => Ok(answer),
            // The operation failed but the session is fine: the client gets a
            // response and may send more.
            Err(Failure::Operation(response)) => Ok((*response, vec![])),
            // There is no response to give.
            Err(Failure::Connection(e)) => Err(e),
        }
    }
}

fn main() {
    let mut transport = OpenocdTransport::default();
    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    if let Err(e) = tock_proto::serve(stdin, stdout, &mut transport) {
        eprintln!("tock-transport-openocd: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(raw: &str) -> TclReply {
        TclReply::parse(raw).expect("parses")
    }

    #[test]
    fn a_page_size_that_cannot_be_an_erase_unit_is_refused() {
        // Zero would divide by zero in sector_span; a non-power-of-two
        // silently straddles real sectors and erases the neighbours.
        assert!(page_size_from(Some(4096)).is_ok());
        assert!(page_size_from(Some(2048)).is_ok());
        assert!(page_size_from(Some(0)).is_err());
        assert!(page_size_from(Some(1000)).is_err());
        assert!(page_size_from(None).is_err());
    }

    #[test]
    fn sector_span_covers_every_touched_sector() {
        // Mid-sector start and end: one sector.
        assert_eq!(sector_span(4096, 0x1010, 16), Some((0x1000, 4096)));
        // Spanning a boundary: both sectors, so both partial ends survive.
        assert_eq!(sector_span(4096, 0x1ff0, 0x20), Some((0x1000, 8192)));
        // Exactly whole sectors: unchanged.
        assert_eq!(sector_span(4096, 0x2000, 8192), Some((0x2000, 8192)));
        // Empty writes touch nothing.
        assert_eq!(sector_span(4096, 0x1010, 0), None);
    }

    #[test]
    fn only_sector_aligned_whole_sector_writes_skip_the_merge() {
        use WritePlan::*;
        assert!(matches!(write_plan(4096, 0x1010, 16), Some(Merge { .. })));
        assert!(matches!(write_plan(4096, 0x1000, 100), Some(Merge { .. })));
        assert!(matches!(write_plan(4096, 0x2000, 8192), Some(Direct)));
        // Nothing to write, whether or not the address is sector-aligned.
        assert_eq!(write_plan(4096, 0x2000, 0), None);
        assert_eq!(write_plan(4096, 0x2010, 0), None);
    }

    #[test]
    fn parses_a_plain_status() {
        let r = reply("0\u{1f}\u{1f}0");
        assert_eq!(r.rc, 0);
        assert!(!r.failed());
    }

    #[test]
    fn reads_the_code_past_a_tcl_error_prefix() {
        // A TCL-level error prefixes the status field with openocd's log line.
        let r = reply("command.c:708: Error: 1\u{1f}Division by zero\u{1f}1\nat file \"x\"");
        assert_eq!(r.rc, 1);
        assert_eq!(r.message(), "Division by zero");
    }

    #[test]
    fn falls_back_to_the_log_when_there_is_no_message() {
        let r = reply("-101\u{1f}\u{1f}Debug Adapter has to be specified");
        assert_eq!(r.rc, -101);
        assert!(r.failed());
        assert_eq!(r.message(), "Debug Adapter has to be specified");
    }

    #[test]
    fn ignores_a_log_that_is_only_the_status_echo() {
        // `capture` returns the script's result when the command logged
        // nothing, and that result is the rc -- which says nothing.
        let r = reply("-4\u{1f}\u{1f}-4");
        assert_eq!(r.message(), "openocd returned -4");
    }

    #[test]
    fn rejects_a_reply_it_cannot_read() {
        assert!(TclReply::parse("not a reply").is_err());
        assert!(TclReply::parse("x\u{1f}y\u{1f}z").is_err());
    }

    #[test]
    fn config_entries_resolve_next_to_the_file_that_named_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("local.cfg"), "").unwrap();

        // Names a real file beside the board file: use it.
        assert_eq!(
            resolve_config_entry("local.cfg", Some(dir.path())),
            dir.path().join("local.cfg").display().to_string()
        );
        // Not a file there: leave it for openocd's script search path.
        assert_eq!(
            resolve_config_entry("board/nordic_nrf52_dk.cfg", Some(dir.path())),
            "board/nordic_nrf52_dk.cfg"
        );
        // No provenance at all, and absolute paths, pass through.
        assert_eq!(resolve_config_entry("local.cfg", None), "local.cfg");
        assert_eq!(
            resolve_config_entry("/a/b.cfg", Some(dir.path())),
            "/a/b.cfg"
        );
    }

    #[test]
    fn config_accepts_a_bare_string_as_well_as_a_list() {
        let bases = std::collections::BTreeMap::new();
        let one = serde_json::json!({"config": "a.cfg"});
        let many = serde_json::json!({"config": ["a.cfg", "b.cfg"]});
        let none = serde_json::json!({});
        assert_eq!(
            resolve_configs(&Options::new(one.as_object().unwrap(), &bases)),
            vec!["a.cfg"]
        );
        assert_eq!(
            resolve_configs(&Options::new(many.as_object().unwrap(), &bases)),
            vec!["a.cfg", "b.cfg"]
        );
        assert!(resolve_configs(&Options::new(none.as_object().unwrap(), &bases)).is_empty());
    }

    /// openocd programs whatever address it is handed, so this check is the
    /// only thing between a wrong address and the kernel. It must come
    /// before the session is touched -- otherwise a bad range on a closed
    /// transport reports "not open" and the real problem goes unsaid.
    #[test]
    fn an_access_outside_flash_is_refused_before_openocd_is_asked() {
        let mut t = OpenocdTransport {
            session: None,
            flash: Some(FlashInfo {
                start: 0x8000_0000,
                size: 0x4_0000,
            }),
        };
        let refusal = |r: Result<_, Failure>| match r {
            Err(Failure::Operation(response)) => match *response {
                Response::Error { message, .. } => message,
                other => panic!("{other:?}"),
            },
            _ => panic!("expected a refusal"),
        };
        // Below the region, past the end, and straddling the end.
        let message = refusal(t.read(0x7fff_ffff, 4));
        assert!(message.contains("outside flash"), "{message}");
        assert!(
            message.contains("0x8000_0000".replace('_', "").as_str()),
            "{message}"
        );
        let message = refusal(t.write(0x8010_0000, &[0u8; 4]).map(|_| Vec::new()));
        assert!(message.contains("outside flash"), "{message}");
        let message = refusal(t.read(0x8003_fffe, 4));
        assert!(message.contains("outside flash"), "{message}");
        // And an access inside it gets past the check, to fail on the session
        // instead -- which is how we know the check is not refusing everything.
        let message = refusal(t.read(0x8002_0000, 4));
        assert!(message.contains("not open"), "{message}");
    }
}
