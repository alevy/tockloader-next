//! Turning the environment into [`Settings`]: which board, which transport,
//! which options, and where each answer came from.
//!
//! Every command starts here, including the ones that go on to serve the
//! protocol rather than speak it.

use std::path::{Path, PathBuf};

use tock_board::{Context, FileSource, Inputs, Layer, ProjectFile, Settings, UserConfig};

use crate::cli::{Format, Global};
use tock_client::{CliError, Code};

struct Fs;

impl FileSource for Fs {
    fn read(
        &self,
        reference: &str,
        relative_to: Option<&str>,
    ) -> Result<(String, String), tock_board::Error> {
        let mut path = PathBuf::from(reference);
        if path.is_relative() {
            if let Some(dir) = relative_to.and_then(|r| Path::new(r).parent()) {
                path = dir.join(path);
            }
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| tock_board::Error::Read(path.display().to_string(), e.to_string()))?;
        Ok((path.display().to_string(), text))
    }
}

/// `TOCK_PROJECT` names the project file outright, the way `TOCK_CONFIG` does
/// for the user config; a value that is not a file means there is no project
/// file, which is how a test or a CI job opts out of the walk up.
///
/// Experimental, and honestly so: both exist because the test suite needed
/// them, and neither has been designed for anyone else.
pub(crate) fn find_project() -> Option<PathBuf> {
    if let Some(pinned) = std::env::var_os("TOCK_PROJECT") {
        return Some(PathBuf::from(pinned)).filter(|p| p.is_file());
    }
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join("Tock.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub(crate) fn user_config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TOCK_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("tock").join("config.toml"))
}

pub(crate) fn load_project() -> Result<Option<(String, ProjectFile)>, CliError> {
    let Some(path) = find_project() else {
        return Ok(None);
    };
    let origin = path.display().to_string();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| CliError::new(Code::Config, format!("{origin}: {e}")))?;
    Ok(Some((origin.clone(), ProjectFile::parse(&origin, &text)?)))
}

/// The project's `[[app]]` entries as absolute paths, empty if there is no
/// project or it declares none. Relative to the file that declared them, not
/// to the working directory.
pub(crate) fn project_apps() -> Result<Vec<PathBuf>, CliError> {
    let Some((origin, project)) = load_project()? else {
        return Ok(Vec::new());
    };
    let dir = Path::new(&origin)
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    Ok(project.apps.iter().map(|a| dir.join(&a.tab)).collect())
}

pub(crate) fn load_settings(g: &Global) -> Result<Settings, CliError> {
    let project = load_project()?;
    let user = match user_config_path().filter(|p| p.is_file()) {
        Some(p) => {
            let origin = p.display().to_string();
            let text = std::fs::read_to_string(&p)
                .map_err(|e| CliError::new(Code::Config, format!("{origin}: {e}")))?;
            Some((origin.clone(), UserConfig::parse(&origin, &text)?))
        }
        None => None,
    };
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let inputs = Inputs {
        flags: Layer {
            board: g.board.clone(),
            transport: g.transport.clone(),
            device: g.device.clone(),
            options: g.options.clone(),
        },
        env: Layer {
            board: env("TOCK_BOARD"),
            transport: env("TOCK_TRANSPORT"),
            device: env("TOCK_DEVICE"),
            // `vars_os`, not `vars`: the latter panics on any variable in the
            // environment that is not valid Unicode, including ones that have
            // nothing to do with us.
            // One variable in the flag's own syntax, one option per line.
            // It used to be `TOCK_OPT_<KEY>`, which had nowhere to put a
            // transport: `TOCK_OPT_SERIAL_PORT` cannot be told from an
            // option named `serial_port`, and `page_size` shows that keys do
            // contain underscores.
            //
            // A line that does not parse is dropped rather than refused: a
            // malformed variable should not stop a command that was not
            // relying on it, and the option it meant to set will be reported
            // missing by whichever transport wanted it.
            options: std::env::var("TOCK_OPTS")
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .filter_map(|line| tock_board::OptionOverride::parse(line).ok())
                .collect(),
        },
        project: project.as_ref().map(|(o, p)| (o.clone(), p)),
        user: user.as_ref().map(|(o, u)| (o.clone(), u)),
    };
    Ok(tock_board::resolve(&inputs, &Fs)?)
}

/// `tock context`: print what was resolved, as a subcommand receives it.
///
/// The counterpart to `tock broker`. A subcommand launched by `tock` reads
/// this from `TOCK_CONTEXT`; one run by hand asks for it.
pub(crate) fn context(g: &Global) -> Result<(), CliError> {
    let context = Context::new(&load_settings(g)?, project_apps()?);
    println!("{}", serde_json::to_string(&context).unwrap());
    Ok(())
}

pub(crate) fn config_show(g: &Global) -> Result<(), CliError> {
    let settings = load_settings(g)?;
    if g.format == Format::Json {
        println!("{}", serde_json::to_string_pretty(&settings).unwrap());
        return Ok(());
    }
    let b = &settings.board;
    let l = &b.layout;
    println!(
        "board      {}  ({})",
        b.info.name, settings.board_file.value
    );
    println!("           from {}", settings.board_file.source);
    println!("arch       {}", b.info.arch);
    println!("flash      {:#x} +{:#x}", l.flash.start.0, l.flash.size.0);
    println!("apps       {:#x} +{:#x}", l.apps.start.0, l.apps.size.0);
    println!("placement  {:?}", l.placement);
    // The routing table, which is the answer to "why did it use that one".
    for (capability, routed) in &settings.transport.routes {
        println!("transport  {capability} -> {}", routed.value);
        println!("           from {}", routed.source);
    }
    for (name, options) in &settings.transport.options {
        if options.is_empty() {
            continue;
        }
        println!("  [{name}]");
        for (k, v) in options {
            println!("    {k} = {}    ({})", v.value, v.source);
        }
    }
    match &settings.device {
        Some(d) => println!("device     {}  (from {})", d.value, d.source),
        None => println!("device     (none)"),
    }
    if let Some(p) = &settings.project {
        println!("project    {p}");
    }
    Ok(())
}
