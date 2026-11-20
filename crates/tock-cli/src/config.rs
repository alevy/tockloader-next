//! Turning the environment into [`Settings`]: which board, which transport,
//! which options, and where each answer came from.
//!
//! Every command starts here, including the ones that go on to serve the
//! protocol rather than speak it.

use std::path::{Path, PathBuf};

use tock_board::{
    Context, Definition, Entry, Environment, FileSource, Inputs, Layer, ProjectFile, Settings,
    Tier, UserConfig,
};

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
        // Canonicalised so a file reached by path compares equal to the same
        // file found by scanning the user's board directory: that is what
        // lets `extends` recognise and skip it, and what stops the `boards`
        // listing counting `./mine.toml` and `mine.toml` as two boards.
        let origin = std::fs::canonicalize(&path).unwrap_or(path);
        Ok((origin.display().to_string(), text))
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

/// `boards/` beside the user config, whether or not either exists.
pub(crate) fn user_boards_dir() -> Option<PathBuf> {
    let path = user_config_path()?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    Some(dir.join("boards"))
}

/// Every `*.toml` in `dir` keyed by its `[board].name`, plus the ones that
/// could not be keyed, as (origin, why), for `tock boards` to report.
///
/// A missing directory is an empty tier, not an error: there is no
/// requirement that anyone keep board files here at all. Deliberately reads
/// each file as a raw [`toml::Table`] rather than [`tock_board::BoardFile::parse`]:
/// a file that `extends` another is incomplete on its own -- its `[layout]`
/// comes from its parent -- so `parse` would reject the headline case this
/// exists to support. All that is needed here is the name.
pub(crate) fn scan_boards(dir: &Path) -> (Tier, Vec<(String, String)>) {
    use std::collections::btree_map::Entry as MapEntry;

    let mut tier = Tier::new();
    let mut broken = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (tier, broken);
    };
    // Sorted so that a collision between two files always names the same
    // two origins, in the same order, run to run.
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();
    paths.sort();

    for path in paths {
        let origin = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| path.clone())
            .display()
            .to_string();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                broken.push((origin, e.to_string()));
                continue;
            }
        };
        let table: toml::Table = match text.parse() {
            Ok(table) => table,
            Err(e) => {
                broken.push((origin, e.to_string()));
                continue;
            }
        };
        let name = table
            .get("board")
            .and_then(toml::Value::as_table)
            .and_then(|b| b.get("name"))
            .and_then(toml::Value::as_str);
        let Some(name) = name else {
            broken.push((origin, "no [board].name".to_string()));
            continue;
        };
        let definition = Definition {
            origin: origin.clone(),
            text,
        };
        match tier.entry(name.to_string()) {
            MapEntry::Vacant(v) => {
                v.insert(Entry::Board(definition));
            }
            // A second file claiming a name already present: there is no
            // text to fall back to once two files disagree about who owns a
            // name, so the entry itself becomes the error.
            MapEntry::Occupied(mut o) => {
                let origins = match o.get() {
                    Entry::Board(existing) => vec![existing.origin.clone(), origin],
                    Entry::Ambiguous(origins) => {
                        let mut origins = origins.clone();
                        origins.push(origin);
                        origins
                    }
                };
                o.insert(Entry::Ambiguous(origins));
            }
        }
    }
    (tier, broken)
}

/// The bundled tier plus the user tier.
pub(crate) fn environment() -> (Environment, Vec<(String, String)>) {
    let mut env = Environment::bundled();
    let (tier, broken) = match user_boards_dir() {
        Some(dir) => scan_boards(&dir),
        None => (Tier::new(), Vec::new()),
    };
    env.push(tier);
    (env, broken)
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

pub(crate) fn load_user() -> Result<Option<(String, UserConfig)>, CliError> {
    let Some(path) = user_config_path().filter(|p| p.is_file()) else {
        return Ok(None);
    };
    let origin = path.display().to_string();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| CliError::new(Code::Config, format!("{origin}: {e}")))?;
    Ok(Some((origin.clone(), UserConfig::parse(&origin, &text)?)))
}

pub(crate) fn load_settings(g: &Global) -> Result<Settings, CliError> {
    let project = load_project()?;
    let user = load_user()?;
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
    let (env, broken) = environment();
    tock_board::resolve(&inputs, &Fs, &env).map_err(|e| {
        // `NoBoard`'s hint, built by `From` below from the bundled list, goes
        // stale the moment a board directory adds its own names -- the same
        // staleness already fixed for `UnknownBoard`. Replace it with one
        // built from what this environment actually defines.
        let no_board = matches!(e, tock_board::Error::NoBoard);
        let mut err: CliError = e.into();
        if no_board {
            let names = env.names();
            err.hints.clear();
            err = err
                .hint(format!(
                    "tock --board <{}|path/to/board.toml> ...",
                    names.join("|")
                ))
                .hint("or add `board = \"...\"` to a Tock.toml in this project");
        }
        // A board file sitting right there, unparsed, is the likeliest
        // reason a hand-written board was not found: say so rather than
        // leaving it to look like the file was never read.
        for (origin, why) in &broken {
            err = err.detail(format!("{origin}: {why}"));
        }
        err
    })
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

/// One board the tool can name without being told where to look.
#[derive(serde::Serialize)]
pub(crate) struct Listed {
    /// What to pass to `--board`.
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Everywhere this board is named: bundled, the user board directory, a
    /// project file, an alias.
    pub from: Vec<String>,
    /// Whether resolution would pick this one, right here.
    pub selected: bool,
    /// Why it could not be read, for a reference that does not resolve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Where it actually resolved to, which is how two names for one board
    /// are recognised as one. Not reported: it is bookkeeping, and the
    /// reference is what a caller would use.
    #[serde(skip)]
    pub resolved: Option<String>,
}

/// `tock boards`: every board the environment defines, plus whatever the
/// configuration in force already names.
///
/// A name is enumerable because `env` is: every tier was scanned to build
/// it, so walking `env.tiers()` costs nothing beyond what resolution already
/// paid. A path is not enumerable the same way -- nothing was ever told
/// where a path-only board lives, unlike a transport, which is found as
/// `tock-transport-*` on `PATH` -- so a board reached only by path still
/// shows up solely because something currently in force names it.
pub(crate) fn boards(g: &Global) -> Result<(), CliError> {
    // The one in force, if there is one. A listing has to work when
    // resolution does not -- "no board selected" is the commonest reason to
    // want it -- so a failure is carried and explained rather than returned.
    let settled = load_settings(g);
    let chosen = settled.as_ref().ok().map(|s| s.board_file.value.clone());
    let unsettled = settled.err();
    let (env, broken) = environment();
    let board_dir = user_boards_dir();
    let board_dir_label = board_dir
        .as_ref()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "the user board directory".to_string());

    let mut listing = Listing {
        rows: Vec::new(),
        env: &env,
        chosen: chosen.as_deref(),
    };

    // Every name any tier defines. A name resolves to the highest tier
    // that defines it, which is the one `add` will find -- so the row says
    // which tier won, and says so when that shadowed a lower one, rather
    // than giving the shadowed definition a row of its own that no
    // reference could reach anyway. An ambiguous name needs no special
    // case here: resolving it is the error, and `add` records that.
    let tiers = env.tiers();
    let label = |i: usize| {
        if i == 0 {
            "bundled".to_string()
        } else {
            board_dir_label.clone()
        }
    };
    for name in env.names() {
        let defining: Vec<usize> = tiers
            .iter()
            .enumerate()
            .filter(|(_, t)| t.contains_key(&name))
            .map(|(i, _)| i)
            .collect();
        let from = match defining.as_slice() {
            [only] => label(*only),
            [lowest, .., top] => format!("{} (shadows {})", label(*top), label(*lowest)),
            // `names` only yields names some tier defines.
            [] => continue,
        };
        listing.add(&name, from, None);
    }
    // A file in the board directory that would not parse at all: no name to
    // key it by, so it is reported by its own path.
    for (origin, why) in &broken {
        listing.rows.push(Listed {
            reference: origin.clone(),
            name: None,
            description: None,
            selected: false,
            resolved: None,
            from: vec![board_dir_label.clone()],
            error: Some(why.clone()),
        });
    }

    // A flag names a board as much as a file does, and leaving it out meant
    // `--board ./mine.toml boards` listed everything except the one it was
    // about, with nothing marked. Typed at a prompt, so relative to nothing.
    if let Some(reference) = &g.board {
        listing.add(reference, "flag --board".into(), None);
    }
    if let Some(reference) = std::env::var("TOCK_BOARD").ok().filter(|v| !v.is_empty()) {
        listing.add(&reference, "env TOCK_BOARD".into(), None);
    }
    if let Some((origin, project)) = load_project()? {
        if let Some(reference) = &project.board {
            listing.add(reference, format!("project {origin}"), Some(&origin));
        }
    }
    if let Some((origin, user)) = load_user()? {
        if let Some(reference) = &user.board {
            listing.add(reference, format!("user config {origin}"), Some(&origin));
        }
        // An alias records only its own name, so its paths are the user
        // config's -- the same rule `resolve` applies.
        for (alias, device) in &user.devices {
            if let Some(reference) = &device.board {
                listing.add(
                    reference,
                    format!("device `{alias}` in user config"),
                    Some(&origin),
                );
            }
        }
    }

    if g.format == Format::Json {
        println!("{}", serde_json::to_string(&listing.rows).unwrap());
        return Ok(());
    }
    for board in &listing.rows {
        let mark = if board.selected { '*' } else { ' ' };
        match &board.error {
            // A TOML parse error spans several lines; only the first --
            // "TOML parse error at line 1, column 6" -- is useful here, and
            // the rest would break this table's columns. JSON above keeps
            // the whole thing.
            Some(why) => println!(
                "{mark} {:<22} ({})",
                board.reference,
                why.lines().next().unwrap_or_default()
            ),
            None => {
                // The description, or failing that what the board calls
                // itself -- `./lora.toml` says nothing, `lora-e5-mini` does.
                // Not for a bundled board, where the name is the reference.
                let what = board
                    .description
                    .as_deref()
                    .or(board.name.as_deref().filter(|n| *n != board.reference))
                    .unwrap_or("");
                println!(
                    "{mark} {:<22} {what:<24} {}",
                    board.reference,
                    board.from.join(", ")
                )
            }
        }
    }
    println!();
    // Why nothing is marked, when nothing is. Resolution settles more than
    // the board, so it can fail with the board perfectly well determined,
    // and a blank column would be the only sign of it.
    if let Some(why) = &unsettled {
        println!("No board is in force here: {}", why.message);
    }
    println!("A path to a board file also works, and is the only way to reach one");
    println!("that is neither bundled nor named above: --board ./my-board.toml");
    println!("Board files placed in {board_dir_label} are listed above by their");
    println!("own name, not their filename");
    Ok(())
}

/// The rows [`boards`] has built so far, plus what [`Listing::add`] needs to
/// resolve a reference against: the environment, and the board currently in
/// force, so a row can mark itself selected.
struct Listing<'a> {
    rows: Vec<Listed>,
    env: &'a Environment,
    chosen: Option<&'a str>,
}

impl Listing<'_> {
    /// Resolve `reference` and add it to `rows`, merging by where it
    /// resolved to so a bundled board a project also names is one row
    /// saying both, rather than two saying half each. A method rather than
    /// a closure over `rows`: the tier walk in [`boards`] also pushes rows
    /// of its own between calls, which a closure holding `rows` borrowed
    /// for its whole lifetime could not allow.
    fn add(&mut self, reference: &str, from: String, relative_to: Option<&str>) {
        let loaded = tock_board::BoardFile::load(reference, relative_to, &Fs, self.env);
        let origin = loaded.as_ref().ok().map(|(_, origin)| origin.clone());
        if let Some(origin) = &origin {
            if let Some(already) = self
                .rows
                .iter_mut()
                .find(|l| l.error.is_none() && l.resolved.as_deref() == Some(origin))
            {
                already.from.push(from);
                return;
            }
        }
        let (name, description, error) = board_summary(loaded.map(|(board, _)| board));
        self.rows.push(Listed {
            reference: reference.to_string(),
            name,
            description,
            selected: origin.is_some() && origin.as_deref() == self.chosen,
            resolved: origin,
            from: vec![from],
            error,
        });
    }
}

/// `(name, description, error)` out of a board that did or did not parse --
/// what a [`Listed`] row needs to say about it.
fn board_summary(
    loaded: Result<tock_board::BoardFile, tock_board::Error>,
) -> (Option<String>, Option<String>, Option<String>) {
    match loaded {
        Ok(board) => (
            Some(board.info.name.clone()),
            Some(board.info.description.clone()).filter(|d| !d.is_empty()),
            None,
        ),
        Err(e) => (None, None, Some(e.to_string())),
    }
}
