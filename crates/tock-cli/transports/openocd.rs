//! `tock-transport-openocd`: drives a long-lived openocd over its TCL RPC port.
//!
//! Nothing here parses openocd's prose. Startup picks the port itself, so
//! readiness is a successful TCP connect and failure is openocd exiting --
//! two unambiguous events rather than a race against stderr. Each TCL command
//! comes back as a status plus whatever openocd logged while running it, so a
//! failure reaches the user in openocd's own words instead of as a number.
//!
//! The options are declared in [`options`], which is what `tock transports`
//! prints and what `tock` holds this end to -- a key not named there never
//! arrives. Two of them want more than a line:
//!
//! - `config`, `target` and `probe` take openocd config files, a string or a
//!   list. An entry naming a file next to the config that declared it is
//!   resolved to that file; anything else passes through for openocd's own
//!   script search path, so `board/nordic_nrf52_dk.cfg` keeps working.
//! - `page_size` is the flash erase unit and has no default, because
//!   guessing one is how a write takes a neighbour with it.
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
    Capability, Connection, ErrorKind, FlashInfo, OptionSpec, Options, ProtoError, Request,
    Response,
};

/// Every option this transport reads. Declared rather than merely
/// documented: `tock` refuses a key that is not here, so this is also what
/// keeps a typo from being swallowed. What a *good* value is stays here,
/// checked at `Open`, because only this end knows.
fn options() -> Vec<OptionSpec> {
    [
        ("page_size", "the flash erase unit, a power of two"),
        ("command", "the openocd executable (default `openocd`)"),
        ("debug_level", "openocd's log verbosity (default 2)"),
        (
            "target",
            "config for the chip: the same for everyone with this board",
        ),
        (
            "probe",
            "config for the debug adapter plugged into this machine",
        ),
        (
            "config",
            "the whole config stage, instead of `target` and `probe`",
        ),
    ]
    .into_iter()
    .map(|(name, description)| OptionSpec {
        name: name.into(),
        description: description.into(),
    })
    .collect()
}

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
#[cfg_attr(test, derive(Debug))]
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
             or give the board file a [transports.openocd] page_size entry",
        )
        .about(["page_size"])
    };
    match configured {
        None => Err(missing()),
        Some(v) if v == 0 || !v.is_power_of_two() => Err(Response::error(
            ErrorKind::Selection,
            format!("page_size {v} cannot be a flash erase unit: it must be a power of two"),
        )
        .with_suggestion("check [transports.openocd] page_size, e.g. page_size = 4096")
        .about(["page_size"])),
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
/// One step of openocd's config stage.
///
/// openocd has no notion of "the config files" and "the commands": `-f FILE`
/// is shorthand for `-c "script FILE"`, and the command line is one ordered
/// queue. The order is load-bearing, not cosmetic -- `transport select swd`
/// must run after the interface file declares the adapter and before the
/// target file builds a TAP with it, or openocd stops with "Debug adapter
/// doesn't support any transports?".
#[derive(Debug, PartialEq)]
enum Step {
    File(String),
    Command(String),
}

impl Step {
    fn args(&self) -> [&str; 2] {
        match self {
            Step::File(path) => ["-f", path],
            Step::Command(command) => ["-c", command],
        }
    }
}

/// Read the config stage from the options.
///
/// Three keys, and the reason there are three is **ownership**:
///
/// - `target` is the chip -- its flash, its TAP. The board author knows it
///   and it is the same for everyone, so it belongs in the board file.
/// - `probe` is what is plugged into *this* machine, and which transport it
///   speaks. Two developers with the same board may have a J-Link and an
///   ST-Link, so it belongs in the user config.
/// - `config` is both at once, for anything that does not decompose.
///
/// They are separate keys rather than one list because option layers merge
/// per key: a board file can ship a sensible `probe` -- an onboard debugger,
/// or the common external one -- and a developer with a different probe
/// overrides that half without restating the chip.
///
/// `probe` runs first, which is also the only order openocd accepts.
fn resolve_configs(options: &Options<'_>) -> Result<Vec<Step>, Failure> {
    let split = ["probe", "target"];
    if options.get("config").is_some() {
        if let Some(key) = split.iter().find(|k| options.get(k).is_some()) {
            return Err(Response::error(
                ErrorKind::Selection,
                format!("openocd `config` and `{key}` cannot both be set"),
            )
            .with_suggestion(
                "`config` is the whole config stage; `probe` and `target` are its two \
                 halves.\nUse one or the other.",
            )
            .about(["config", *key])
            .into());
        }
        return steps(options, "config");
    }
    let mut out = steps(options, "probe")?;
    out.extend(steps(options, "target")?);
    Ok(out)
}

/// One key's worth of steps. A bare string is a config file, which is the
/// common case and stays terse; a table carries either `command` (TCL, run
/// as given) or `file` (the long form of the string).
fn steps(options: &Options<'_>, key: &str) -> Result<Vec<Step>, Failure> {
    let Some(value) = options.get(key) else {
        return Ok(Vec::new());
    };
    let base = options.base_dir(key);
    let entries: Vec<&serde_json::Value> = match value {
        serde_json::Value::Array(entries) => entries.iter().collect(),
        single => vec![single],
    };
    entries
        .into_iter()
        .map(|entry| config_step(entry, base.as_deref()))
        .collect()
}

fn config_step(entry: &serde_json::Value, base: Option<&Path>) -> Result<Step, Failure> {
    if let Some(path) = entry.as_str() {
        return Ok(Step::File(resolve_config_entry(path, base)));
    }
    if let Some(table) = entry.as_object() {
        if let Some(command) = table.get("command").and_then(|v| v.as_str()) {
            return Ok(Step::Command(command.to_string()));
        }
        if let Some(path) = table.get("file").and_then(|v| v.as_str()) {
            return Ok(Step::File(resolve_config_entry(path, base)));
        }
    }
    // Not skipped: a dropped entry means openocd runs without the step and
    // fails later saying something unrelated, which is the worst outcome
    // available. A typo should say it is a typo.
    Err(Response::error(
        ErrorKind::Selection,
        format!("openocd config entry {entry} is neither a path nor a command"),
    )
    .with_suggestion(
        "a config file is a bare string: \"interface/cmsis-dap.cfg\"\n\
         a TCL command is a table: { command = \"transport select swd\" }",
    )
    .into())
}

/// How to leave the target, given whether flash was programmed.
///
/// Read-only work puts it back as it was found, so looking at a running
/// board does not disturb it. Programming resets, because resuming into code
/// that changed underneath is not a state the device was ever in -- and
/// after installing an app, running it is the point.
fn parting_command(wrote: bool) -> &'static str {
    if wrote { "reset run" } else { "resume" }
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
    /// Whether anything was programmed, which decides how the target is left
    /// at `Close`. See [`Session::release`].
    wrote: bool,
}

impl Session {
    fn start(
        command: &str,
        configs: &[Step],
        debug_level: i64,
        page_size: u64,
    ) -> Result<Session, ProtoError> {
        // Choosing the port here means there is nothing to discover later.
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = probe.local_addr()?.port();
        drop(probe);

        let log = NamedTempFile::new()?;
        let mut openocd = Command::new(command)
            .args(configs.iter().flat_map(Step::args))
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
            wrote: false,
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
        self.wrote = true;
        Ok(())
    }

    /// Give the target back, then ask openocd to go.
    ///
    /// The core is halted for the life of the session, so leaving it that
    /// way hands back a board that does not run -- and a halted core stays
    /// halted once the debugger goes away. The protocol says `Close`
    /// releases the device; this is what releasing it means.
    ///
    /// Which of the two ways depends on what we did. Having only read, the
    /// target is put back as it was found, so inspecting a running board
    /// does not disturb it. Having programmed flash, it is reset, because
    /// resuming into code that changed underneath is not a state the device
    /// was ever in -- and after installing an app, running it is the point.
    ///
    /// Best effort throughout: this runs while shutting down, and a board
    /// that will not resume is not a reason to fail a command that worked.
    fn release(&mut self) {
        let _ = self.tcl_cmd(parting_command(self.wrote));
        // Ask openocd to go rather than killing it, so it lets go of the
        // adapter cleanly. It stops answering at once, so the reply, if any,
        // is not worth reading.
        let _ = self.tcl_cmd("shutdown");
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
    /// Release the target, however the session ended.
    ///
    /// Here rather than only on `Close`, because `Close` is not reliably the
    /// way a session ends. `tock` answers its own client's `Close` locally
    /// -- the client did not open this transport and does not get to say
    /// when it shuts -- and then leaves by `process::exit`, which runs no
    /// destructors, so the transport's own `Close` is never sent. What the
    /// transport actually sees is end of stream. Dropping is the one event
    /// common to every ending, so the release belongs here.
    ///
    /// The kill stays as the backstop: if openocd ignored `shutdown`, or
    /// never got it, it would otherwise keep the debug adapter and the next
    /// run would fail with the probe busy.
    fn drop(&mut self) {
        self.release();
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
        let configs = resolve_configs(&options)?;
        if configs.is_empty() {
            return Err(Response::error(
                ErrorKind::Selection,
                "the openocd transport has nothing to configure it with",
            )
            .with_suggestion(
                "the board file should give [transports.openocd] a `target`\n\
                 your user config should give it a `probe` for what you have plugged in,\n\
                 e.g. [transports.openocd] probe = [\"interface/jlink.cfg\"]",
            )
            .about(["target", "probe", "config"])
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
                    options: options(),
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
                // Dropping it releases the target and reaps openocd.
                self.session = None;
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
#[path = "openocd/tests.rs"]
mod tests;
