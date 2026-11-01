//! `tock-transport-openocd`: tock-cli transport using openocd

use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
};

use tempfile::NamedTempFile;
use tock_proto::{Capability, ErrorKind, Handler, Request, Response};

pub struct OpenocdTransport {
    openocd: Option<Child>,
    tcl_conn: Option<BufReader<TcpStream>>,
}

impl OpenocdTransport {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            openocd: None,
            tcl_conn: None,
        })
    }

    fn spawn_openocd(&mut self, configs: &[String]) -> Response {
        let tcp_probe = TcpListener::bind("localhost:0").expect("oops");
        let port = tcp_probe.local_addr().expect("oops").port();
        drop(tcp_probe);

        let mut cmd = Command::new("openocd");
        cmd.args(configs.iter().flat_map(|c| ["-f", c]))
            .arg("-c")
            .arg(format!("tcl_port {port}"))
            .arg("-c")
            .arg("debug_level 4; init; halt")
            .stderr(Stdio::piped());
        match cmd.spawn() {
            Ok(mut pid) => loop {
                if let Ok(tcl_conn) = TcpStream::connect(("localhost", port)) {
                    self.tcl_conn = Some(BufReader::new(tcl_conn));
                    self.openocd = Some(pid);
                    return Response::Ok;
                } else {
                    if pid.try_wait().expect("oops").is_some() {
                        let mut buf = String::new();
                        pid.stderr.unwrap().read_to_string(&mut buf).unwrap();
                        return Response::error(ErrorKind::Device, buf);
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            },
            Err(e) => Response::error(ErrorKind::Device, format!("{e}")),
        }
    }

    fn tcl_cmd(&mut self, cmd: &str) -> Option<std::io::Result<(bool, String)>> {
        self.tcl_conn.as_mut().map(|sock| {
            let wrapped = format!(
                r#"set log [capture {{set rc [catch {{{cmd}}} msg]}}]; return "$rc\x1f$log""#
            );
            sock.get_mut().write_all(wrapped.as_bytes())?;
            sock.get_mut().write_all(&[0x1a])?;
            sock.get_mut().flush()?;
            let mut buf = Vec::new();
            sock.read_until(0x1a, &mut buf)?;
            buf.pop(); // drop the terminator
            let response_line = String::from_utf8_lossy(&buf).into_owned();
            let (status, response) =
                response_line
                    .split_once('\x1f')
                    .ok_or(std::io::Error::other(format!(
                        "unexpected tcl response: {response_line}"
                    )))?;
            eprintln!("Status {status}");
            Ok((status == "0", response.to_string()))
        })
    }
}

impl Handler for OpenocdTransport {
    fn handle(&mut self, request: Request, payload: Vec<u8>) -> (Response, Vec<u8>) {
        match request {
            Request::Hello { .. } => (
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
            ),
            Request::Open { options, .. } => {
                let configs: Vec<String> = options
                    .get("config")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or(vec![]);
                (self.spawn_openocd(&configs), vec![])
            }
            Request::Read { address, len } => {
                let mut scratch_file = NamedTempFile::new().unwrap();
                let d = self.tcl_cmd(
                    format!(
                        "dump_image {} {address:#x} {len}",
                        scratch_file.path().to_string_lossy()
                    )
                    .as_str(),
                );
                if let Some(Ok((true, _resp))) = d {
                    let mut buf = vec![];
                    let _ = scratch_file.read_to_end(&mut buf);
                    (Response::Data, buf)
                } else {
                    (
                        Response::error(ErrorKind::InvalidRequest, "not ready"),
                        vec![],
                    )
                }
            }
            Request::Write { address } => {
                let mut scratch_file = NamedTempFile::new().unwrap();
                scratch_file.write_all(&payload).unwrap();
                scratch_file.flush().unwrap();

                let d = self.tcl_cmd(
                    format!(
                        "flash write_image erase {} {address:#x} bin",
                        scratch_file.path().to_string_lossy()
                    )
                    .as_str(),
                );
                if let Some(Ok((true, _resp))) = d {
                    (Response::Ok, vec![])
                } else {
                    (
                        Response::error(ErrorKind::InvalidRequest, "not ready"),
                        vec![],
                    )
                }
            }
            Request::Reset => {
                let d = self.tcl_cmd("reset");
                if let Some(Ok((true, _resp))) = d {
                    (Response::Ok, vec![])
                } else {
                    (
                        Response::error(ErrorKind::InvalidRequest, "not ready"),
                        vec![],
                    )
                }
            }
            Request::Close => {
                if let Some(mut child) = self.openocd.take() {
                    match child.kill() {
                        Ok(()) => (Response::Ok, vec![]),
                        Err(e) => (
                            Response::error(
                                ErrorKind::Device,
                                format!("openocd failed to close {child:?}: {e}"),
                            ),
                            vec![],
                        ),
                    }
                } else {
                    (
                        Response::error(
                            ErrorKind::InvalidRequest,
                            "openocd transport has nothing to close".to_string(),
                        ),
                        vec![],
                    )
                }
            }
            other => (
                Response::error(
                    ErrorKind::Unsupported,
                    format!("openocd transport does not support {other:?}"),
                ),
                vec![],
            ),
        }
    }
}

fn main() -> std::io::Result<()> {
    let mut transport = OpenocdTransport::new()?;
    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    if let Err(e) = tock_proto::serve(stdin, stdout, &mut transport) {
        eprintln!("tock-transport-openocd: {e}");
        std::process::exit(1);
    }
    Ok(())
}
