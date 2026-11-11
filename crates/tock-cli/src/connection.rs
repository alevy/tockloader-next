//! The core's side of the connection to a device: spawning a
//! `tock-transport-<name>` executable and memory operations on top of the
//! protocol. The core has no transports of its own.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use tock_board::Settings;
use tock_proto::{Client, Connection, FlashInfo, Request, Response, Traced};

use tock_client::{CliError, Code, Spawned, proto};

/// Open the transport named in the context. Every transport is an executable
/// `tock-transport-<name>`, found next to `tock` or on `PATH` -- including the
/// `file` transport, which ships alongside `tock` but is not special.
pub fn open_connection(settings: &Settings, trace: bool) -> Result<Box<dyn Connection>, CliError> {
    let name = &settings.transport.name.value;
    let exe = format!("tock-transport-{name}");
    let path = crate::external::find_executable(&exe).ok_or_else(|| {
        CliError::new(
            Code::Config,
            format!(
                "transport `{name}` (from {}) needs `{exe}`, which was not found \
                 next to `tock` or on PATH",
                settings.transport.name.source
            ),
        )
        .hint(format!("install it, e.g. `cargo install {exe}`"))
    })?;
    let mut child = Command::new(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            CliError::new(
                Code::Device,
                format!("cannot start {}: {e}", path.display()),
            )
        })?;
    let reader = child.stdout.take().expect("piped");
    let writer = child.stdin.take().expect("piped");
    let spawned = Spawned::new(reader, writer, child);
    Ok(if trace {
        Box::new(Traced {
            peer: "transport",
            inner: Box::new(spawned),
        })
    } else {
        Box::new(spawned)
    })
}

/// Spawn the transport the context names, greet it, and open it.
///
/// The result is an ordinary [`Client`] -- the same thing a subcommand gets
/// from the connection it inherits. The difference is only that we started
/// the transport and it did not.
pub fn open(settings: &Settings, trace: bool) -> Result<Client, CliError> {
    let connection = open_connection(settings, trace)?;
    let layout = &settings.board.layout;
    let flash = FlashInfo {
        start: layout.flash.start.0,
        size: layout.flash.size.0,
    };
    let mut device =
        Client::open(connection, concat!("tock ", env!("CARGO_PKG_VERSION"))).map_err(proto)?;
    // Before `Open`, so a typo costs a greeting and not a halted target.
    refuse_undeclared(&device, settings)?;

    let options = serde_json::to_value(settings.transport_options()).unwrap();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // The raw connection, not `Client::call`: a refusal carries the option
    // keys it is about, and collapsing it to a `ProtoError` would drop them
    // before we could say where those options were written.
    let (response, _) = device
        .connection
        .call(
            Request::Open {
                board: settings.board.info.name.clone(),
                arch: settings.board.info.arch.clone(),
                flash,
                options: options.as_object().cloned().unwrap_or_default(),
                option_bases: settings.option_bases(&cwd),
            },
            vec![],
        )
        .map_err(proto)?;
    match response {
        Response::Error {
            kind,
            message,
            suggestion,
            details,
            keys,
        } => Err(refused(settings, kind, message, suggestion, details, keys)),
        _ => Ok(device),
    }
}

/// Refuse an option the transport never said it reads.
///
/// Only this end can: it resolved the options and knows the whole set, while
/// the transport sees a map and cannot tell a key it ignores from a key that
/// was never meant for it. A silently ignored typo is the one failure all the
/// provenance in the world cannot help with.
fn refuse_undeclared(device: &Client, settings: &Settings) -> Result<(), CliError> {
    let given: Vec<&str> = settings
        .transport
        .options
        .keys()
        .map(String::as_str)
        .collect();
    let unknown = device.undeclared(given);
    let Some(first) = unknown.first() else {
        return Ok(());
    };
    let name = &settings.transport.name.value;
    let mut error = CliError::new(
        Code::Config,
        format!(
            "transport `{name}` does not read the option `{first}`{}",
            match unknown.len() {
                1 => String::new(),
                n => format!(" (or {} other{})", n - 1, if n == 2 { "" } else { "s" }),
            }
        ),
    );
    if let Some(source) = settings.transport.options.get(first) {
        error = error.detail(format!("`{first}` came from {}", source.source));
    }
    if let Some(near) = nearest(first, device) {
        error = error.hint(format!("did you mean `{near}`?"));
    }
    let known: Vec<&str> = device.options.iter().map(|o| o.name.as_str()).collect();
    error = error.hint(match known.is_empty() {
        true => format!("`{name}` reads no options at all"),
        false => format!("`{name}` reads: {}", known.join(", ")),
    });
    Err(error)
}

/// The declared option a typo most plausibly meant.
///
/// Edit distance, because the typos that matter are a dropped letter
/// (`crate` for `create`) or a swapped pair, and neither shows up as a
/// shared prefix. Bounded at a third of the name's length so a short name
/// cannot match everything: `path` and `probe` stay distinct.
fn nearest(typo: &str, device: &Client) -> Option<String> {
    device
        .options
        .iter()
        .map(|option| (distance(typo, &option.name), option.name.clone()))
        .filter(|(d, name)| *d * 3 <= name.len().max(typo.len()))
        .min()
        .map(|(_, name)| name)
}

/// Levenshtein distance, two rows at a time.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut row = vec![0; b.len() + 1];
    for (i, x) in a.iter().enumerate() {
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let substitute = prev[j] + usize::from(x != y);
            row[j + 1] = substitute.min(prev[j + 1] + 1).min(row[j] + 1);
        }
        std::mem::swap(&mut prev, &mut row);
    }
    prev[b.len()]
}

/// A refusal from `Open`, with the provenance of whatever options it named.
///
/// The transport knows which key is wrong and why; only this end knows which
/// of five layers wrote it. Neither sentence is much use without the other.
fn refused(
    settings: &Settings,
    kind: tock_proto::ErrorKind,
    message: String,
    suggestion: Option<String>,
    details: Vec<String>,
    keys: Vec<String>,
) -> CliError {
    let name = &settings.transport.name.value;
    let mut error = CliError::new(tock_client::code_for(kind), format!("{name}: {message}"));
    for detail in details {
        error = error.detail(detail);
    }
    for key in &keys {
        if let Some(sourced) = settings.transport.options.get(key) {
            error = error.detail(format!(
                "{key} = {}, from {}",
                sourced.value, sourced.source
            ));
        } else {
            error = error.detail(format!("`{key}` is not set by any flag, file or alias"));
        }
    }
    for line in suggestion.iter().flat_map(|s| s.lines()) {
        error = error.hint(line);
    }
    error
}

/// `tock transports`: what is installed here, and what each one reads.
///
/// Every transport is asked, because only it knows. `Hello` is answered
/// before a transport touches a device, so this works with nothing plugged
/// in -- which is the point, since it is what you run to find out what to
/// plug in.
pub fn transports(g: &crate::cli::Global) -> Result<(), CliError> {
    let names = crate::external::transports();
    if names.is_empty() {
        println!("no transports found next to `tock` or on PATH");
        return Ok(());
    }
    let mut report = Vec::new();
    for name in names {
        report.push(greet(&name));
    }
    if g.format == crate::cli::Format::Json {
        println!("{}", serde_json::to_string(&report).unwrap());
        return Ok(());
    }
    for entry in &report {
        match &entry.error {
            Some(why) => println!("{:<14} ({why})", entry.name),
            None => {
                let can: Vec<String> = entry.capabilities.iter().map(|c| c.name()).collect();
                println!("{:<14} {:<9} {}", entry.name, entry.version, can.join(" "));
                for option in &entry.options {
                    println!("  {:<12} {}", option.name, option.description);
                }
            }
        }
    }
    Ok(())
}

/// One transport's answer to `Hello`, or why it could not give one.
#[derive(serde::Serialize)]
pub struct Greeting {
    pub name: String,
    pub version: String,
    pub capabilities: Vec<tock_proto::Capability>,
    pub options: Vec<tock_proto::OptionSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn greet(name: &str) -> Greeting {
    let mut greeting = Greeting {
        name: name.to_string(),
        version: String::new(),
        capabilities: Vec::new(),
        options: Vec::new(),
        error: None,
    };
    let exe = format!("tock-transport-{name}");
    let Some(path) = crate::external::find_executable(&exe) else {
        greeting.error = Some(format!("{exe} disappeared"));
        return greeting;
    };
    let spawned = Command::new(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else {
        greeting.error = Some(format!("cannot start {}", path.display()));
        return greeting;
    };
    let reader = child.stdout.take().expect("piped");
    let writer = child.stdin.take().expect("piped");
    let connection = Box::new(Spawned::new(reader, writer, child));
    match Client::open(connection, concat!("tock ", env!("CARGO_PKG_VERSION"))) {
        Ok(client) => {
            greeting.version.clone_from(&client.version);
            greeting.capabilities.clone_from(&client.capabilities);
            greeting.options.clone_from(&client.options);
        }
        Err(e) => greeting.error = Some(e.to_string()),
    }
    greeting
}
