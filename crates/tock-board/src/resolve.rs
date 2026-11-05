//! Combine flags, environment, project file, user config and board file into
//! one [`Context`].
//!
//! Precedence, highest first, for each setting:
//!
//! 1. command-line flag
//! 2. environment variable (`TOCK_BOARD`, `TOCK_TRANSPORT`, `TOCK_DEVICE`, and
//!    `TOCK_OPT_<KEY>` for transport options)
//! 3. the selected device alias from the user config
//! 4. project file (`Tock.toml`)
//! 5. user config
//! 6. board file
//!
//! Transport options layer per key, highest first: `--opt`, `TOCK_OPT_<KEY>`,
//! device alias, `[transports.<name>]` in the project file,
//! `[transports.<name>]` in the user config, `[connection.<name>]` in the
//! board file.
//!
//! Every resolved value carries its [`Source`], so `tock config show` can
//! explain all of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use tock_image::{Placement, Region};

use crate::{Board, Error, FileSource, ProjectFile, UserConfig};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "from", content = "at", rename_all = "snake_case")]
pub enum Source {
    Flag(String),
    Env(String),
    Device(String),
    Project(String),
    User(String),
    Board(String),
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Flag(s) => write!(f, "flag {s}"),
            Source::Env(s) => write!(f, "env {s}"),
            Source::Device(s) => write!(f, "device `{s}` in user config"),
            Source::Project(s) => write!(f, "project {s}"),
            Source::User(s) => write!(f, "user config {s}"),
            Source::Board(s) => write!(f, "board {s}"),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Sourced<T> {
    pub value: T,
    pub source: Source,
}

/// Settings given directly, by flags or environment variables.
#[derive(Clone, Debug, Default)]
pub struct Layer {
    pub board: Option<String>,
    pub transport: Option<String>,
    pub device: Option<String>,
    /// Transport options as `key=value` (flags only).
    pub options: Vec<(String, String)>,
}

pub struct Inputs<'a> {
    pub flags: Layer,
    pub env: Layer,
    /// Origin (path) and contents of the project file, if one was found.
    pub project: Option<(String, &'a ProjectFile)>,
    pub user: Option<(String, &'a UserConfig)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TransportContext {
    pub name: Sourced<String>,
    pub options: BTreeMap<String, Sourced<toml::Value>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Context {
    /// Where the board definition was loaded from.
    pub board_file: Sourced<String>,
    pub board: Board,
    pub transport: TransportContext,
    pub device: Option<Sourced<String>>,
    pub project: Option<String>,
    /// Origin of the user config, if one was read. A device alias records only
    /// the alias name in its [`Source`], so this is where an option that came
    /// from an alias was actually written.
    pub user: Option<String>,
}

impl Context {
    /// Transport options as a plain table, for handing to a transport.
    pub fn transport_options(&self) -> toml::Table {
        self.transport
            .options
            .iter()
            .map(|(k, v)| (k.clone(), v.value.clone()))
            .collect()
    }
    /// The directory each resolved option was written in, for a transport
    /// that must resolve a relative path.
    ///
    /// This crate never decides what an option *means*, so provenance is all
    /// it can offer: `path = "flash.bin"` is relative to whichever file said
    /// so, and only [`Sourced`] knows which that was. Absolute, because a
    /// transport or a `tock broker` client does not reliably share `cwd`.
    pub fn option_bases(&self, cwd: &Path) -> BTreeMap<String, String> {
        let dir_of = |origin: Option<&String>| {
            // A board reference like `bundled:nrf52840dk` is not a file and
            // has no directory; values from one cannot be relative paths.
            let Some(path) = origin.filter(|o| !o.starts_with("bundled:")).map(Path::new) else {
                return cwd.to_path_buf();
            };
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            absolute.parent().unwrap_or(cwd).to_path_buf()
        };

        self.transport
            .options
            .iter()
            .map(|(key, sourced)| {
                let origin = match &sourced.source {
                    // Flags and the environment are written at a prompt, not
                    // in a file, so their base is wherever we were run.
                    Source::Flag(_) | Source::Env(_) => None,
                    // An alias records only its own name, so the file it was
                    // written in is the user config.
                    Source::Device(_) => self.user.as_ref(),
                    Source::Project(o) | Source::User(o) | Source::Board(o) => Some(o),
                };
                (key.clone(), dir_of(origin).display().to_string())
            })
            .collect()
    }
}

/// What `tock` hands a subcommand, as JSON in `TOCK_CONTEXT`.
///
/// Deliberately narrower than [`Context`]: everything a client needs to plan
/// against this board, and nothing about how the connection was made. A
/// client never opens a transport -- it reaches the device through
/// `tock broker` -- so transport names, options and their provenance are none
/// of its business, and leaving them out keeps this a contract we can keep
/// rather than a dump of whatever `Context` happens to hold.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resolved {
    pub board: String,
    pub arch: String,
    /// Where apps live, and the rules for placing them there.
    pub apps: Region,
    pub placement: Placement,
    /// RAM available to an app, when the board file says. Used to rule out
    /// fixed-address builds that cannot run here.
    pub app_ram: Option<Region>,
    /// The project's `[[app]]` entries, resolved to absolute paths, so a
    /// client installing "whatever this project declares" need not find or
    /// parse `Tock.toml` itself.
    pub project_apps: Vec<PathBuf>,
}

impl Resolved {
    pub fn new(context: &Context, project_apps: Vec<PathBuf>) -> Resolved {
        Resolved {
            board: context.board.board.name.clone(),
            arch: context.board.board.arch.clone(),
            apps: context.board.app_region(),
            placement: context.board.layout.placement.clone(),
            // As a Region, not the bare tuple the planner takes: `[start,
            // size]` in JSON is a worse contract than named fields.
            app_ram: context
                .board
                .app_ram()
                .map(|(start, size)| Region { start, size }),
            project_apps,
        }
    }
}

pub fn resolve(inputs: &Inputs, files: &dyn FileSource) -> Result<Context, Error> {
    let project = inputs.project.as_ref();
    let user = inputs.user.as_ref();

    // The device first, since an alias can supply the board and transport.
    let device = pick(&[
        (
            inputs.flags.device.as_ref(),
            Source::Flag("--device".into()),
        ),
        (
            inputs.env.device.as_ref(),
            Source::Env("TOCK_DEVICE".into()),
        ),
        (
            project.and_then(|(_, p)| p.device.as_ref()),
            project.map_or(Source::Project(String::new()), |(o, _)| {
                Source::Project(o.clone())
            }),
        ),
    ]);
    let alias = device.as_ref().and_then(|d| {
        user.and_then(|(_, u)| u.devices.get(&d.value))
            .map(|a| (d.value.clone(), a))
    });
    let alias_source =
        || Source::Device(alias.as_ref().map(|(n, _)| n.clone()).unwrap_or_default());
    let project_source = || Source::Project(project.map(|(o, _)| o.clone()).unwrap_or_default());

    let board_ref = pick(&[
        (inputs.flags.board.as_ref(), Source::Flag("--board".into())),
        (inputs.env.board.as_ref(), Source::Env("TOCK_BOARD".into())),
        (
            alias.as_ref().and_then(|(_, a)| a.board.as_ref()),
            alias_source(),
        ),
        (
            project.and_then(|(_, p)| p.board.as_ref()),
            project_source(),
        ),
    ])
    .ok_or(Error::NoBoard)?;
    let relative_to = match &board_ref.source {
        Source::Project(origin) => Some(origin.as_str()),
        Source::User(origin) => Some(origin.as_str()),
        _ => None,
    };
    let (board, board_origin) = Board::load(&board_ref.value, relative_to, files)?;

    let transport = pick(&[
        (
            inputs.flags.transport.as_ref(),
            Source::Flag("--transport".into()),
        ),
        (
            inputs.env.transport.as_ref(),
            Source::Env("TOCK_TRANSPORT".into()),
        ),
        (
            alias.as_ref().and_then(|(_, a)| a.transport.as_ref()),
            alias_source(),
        ),
        (
            project.and_then(|(_, p)| p.transport.as_ref()),
            project_source(),
        ),
    ])
    .or_else(|| {
        board.default_transport().map(|t| Sourced {
            value: t.to_string(),
            source: Source::Board(board_origin.clone()),
        })
    })
    .ok_or_else(|| Error::NoTransport(board.board.name.clone()))?;

    // Options, lowest precedence first so later layers overwrite.
    let mut options = BTreeMap::new();
    let mut layer = |table: Option<&toml::Table>, source: Source| {
        for (k, v) in table.into_iter().flatten() {
            options.insert(
                k.clone(),
                Sourced {
                    value: v.clone(),
                    source: source.clone(),
                },
            );
        }
    };
    layer(
        board.transport_options(&transport.value),
        Source::Board(board_origin.clone()),
    );
    if let Some((origin, u)) = user {
        layer(
            u.transports.get(&transport.value),
            Source::User(origin.clone()),
        );
    }
    if let Some((_, p)) = project {
        layer(p.transports.get(&transport.value), project_source());
    }
    if let Some((_, a)) = &alias {
        layer(Some(&a.options), alias_source());
    }
    for (k, v) in &inputs.env.options {
        options.insert(
            k.clone(),
            Sourced {
                value: parse_option_value(v),
                source: Source::Env(env_option_var(k)),
            },
        );
    }
    for (k, v) in &inputs.flags.options {
        options.insert(
            k.clone(),
            Sourced {
                value: parse_option_value(v),
                source: Source::Flag(format!("--opt {k}")),
            },
        );
    }

    Ok(Context {
        board_file: Sourced {
            value: board_origin,
            source: board_ref.source,
        },
        board,
        transport: TransportContext {
            name: transport,
            options,
        },
        device,
        project: project.map(|(o, _)| o.clone()),
        user: user.map(|(o, _)| o.clone()),
    })
}

/// Accept TOML values (numbers, booleans, arrays); anything else is a string.
fn parse_option_value(raw: &str) -> toml::Value {
    toml::from_str::<toml::Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|mut t| t.remove("v"))
        .unwrap_or_else(|| toml::Value::String(raw.to_string()))
}

/// The environment variable an option key is read from. Option keys are
/// snake_case, so this is case alone: `chip_name` is `TOCK_OPT_CHIP_NAME`,
/// and every key is reachable from the environment.
fn env_option_var(key: &str) -> String {
    format!("TOCK_OPT_{}", key.to_uppercase())
}

/// The option key an environment variable name carries, if it is one.
pub fn env_option_key(var: &str) -> Option<String> {
    var.strip_prefix("TOCK_OPT_")
        .filter(|rest| !rest.is_empty())
        .map(|rest| rest.to_lowercase())
}

fn pick(layers: &[(Option<&String>, Source)]) -> Option<Sourced<String>> {
    layers.iter().find_map(|(v, s)| {
        v.map(|v| Sourced {
            value: v.clone(),
            source: s.clone(),
        })
    })
}

#[cfg(test)]
mod tests;
