//! Combine flags, environment, project file, user config and board file into
//! one [`Settings`].
//!
//! The environment layer is **experimental**. It duplicates what flags and
//! files already say, and is the only layer that cannot express everything
//! they can: `TOCK_OPT_SERIAL_PORT` cannot be told from an option named
//! `serial_port`, so it reaches the default transport and no other. Whether
//! it earns its place is open -- see the README.
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
//! `[transports.<name>]` in the user config, the same table in the
//! board file.
//!
//! Every resolved value carries its [`Source`], so `tock config show` can
//! explain all of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use tock_image::{Placement, Region};

use crate::{BoardFile, DEFAULT_ROUTE, Error, FileSource, ProjectFile, Transport, UserConfig};

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

/// One option set from outside a file.
///
/// `transport` names which transport it is for, the way the table header
/// does in a file: `--opt serial.port=X` is `[transports.serial] port = X`.
/// Unqualified means the transport serving [`DEFAULT_ROUTE`], which is what
/// someone overriding the obvious thing means.
#[derive(Clone, Debug)]
pub struct OptionOverride {
    pub transport: Option<String>,
    pub key: String,
    pub value: String,
}

impl OptionOverride {
    /// Whether this applies to the transport `name`, given what `default`
    /// routes to.
    fn applies_to(&self, name: &str, default: Option<&str>) -> bool {
        match &self.transport {
            Some(qualified) => qualified == name,
            None => default == Some(name),
        }
    }

    /// How it is named when something has to be said about where it came
    /// from.
    fn written(&self) -> String {
        match &self.transport {
            Some(transport) => format!("{transport}.{}", self.key),
            None => self.key.clone(),
        }
    }
}

/// Settings given directly, by flags or environment variables.
#[derive(Clone, Debug, Default)]
pub struct Layer {
    pub board: Option<String>,
    pub transport: Option<String>,
    pub device: Option<String>,
    /// Transport options given outside a file.
    pub options: Vec<OptionOverride>,
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
    /// Capability name, or [`DEFAULT_ROUTE`], to the transport that serves
    /// it. Layered per key, so a board file's `default` survives a user
    /// config overriding only `console`.
    pub routes: BTreeMap<String, Sourced<String>>,
    /// Resolved options, per transport named by a route. Only the routed
    /// ones: a `[transports.probe-rs]` table for a transport nothing routes
    /// to is never consulted, which is what lets a board file carry options
    /// for transports that are not installed.
    pub options: BTreeMap<String, BTreeMap<String, Sourced<toml::Value>>>,
}

impl TransportContext {
    /// The transport serving `capability`, falling back to the default.
    pub fn route(&self, capability: &str) -> Option<&Sourced<String>> {
        self.routes
            .get(capability)
            .or_else(|| self.routes.get(DEFAULT_ROUTE))
    }

    /// Every transport any route names, in a stable order.
    pub fn routed(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.routes.values().map(|s| s.value.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Settings {
    /// Where the board definition was loaded from.
    pub board_file: Sourced<String>,
    pub board: BoardFile,
    pub transport: TransportContext,
    pub device: Option<Sourced<String>>,
    pub project: Option<String>,
    /// Origin of the user config, if one was read. A device alias records only
    /// the alias name in its [`Source`], so this is where an option that came
    /// from an alias was actually written.
    pub user: Option<String>,
}

impl Settings {
    /// One transport's options as a plain table, for handing to it.
    pub fn transport_options(&self, name: &str) -> toml::Table {
        self.transport
            .options
            .get(name)
            .into_iter()
            .flatten()
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
    pub fn option_bases(&self, name: &str, cwd: &Path) -> BTreeMap<String, String> {
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
            .get(name)
            .into_iter()
            .flatten()
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

/// What a subcommand is told about the board itself. A struct rather than
/// bare fields so more can be said about a board later without changing the
/// shape of everything around it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Board {
    pub name: String,
    /// The TBF architecture tag used to pick builds out of a TAB. A build
    /// convention, not the chip's ISA: a Cortex-M33 board whose apps are
    /// built by elf2tab as `cortex-m4` says `cortex-m4` here.
    pub arch: String,
}

/// What `tock` hands a subcommand, as JSON in `TOCK_CONTEXT`.
///
/// Deliberately narrower than [`Settings`]: everything a client needs to plan
/// against this board, and nothing about how the connection was made. A
/// client never opens a transport -- it reaches the device through
/// `tock broker` -- so transport names, options and their provenance are none
/// of its business, and leaving them out keeps this a contract we can keep
/// rather than a dump of whatever `Settings` happens to hold.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Context {
    pub board: Board,
    /// Where apps live, and the rules for placing them there.
    pub apps: Region,
    pub placement: Placement,
    /// RAM available to an app, as the board file declares it. A guess: the
    /// kernel's own data comes out of the bottom of this, so the real figure
    /// is smaller and depends on the kernel build. A client that can reach
    /// the device should prefer what the kernel reports.
    pub app_ram: Option<Region>,
    /// The whole flash the transport was opened for.
    ///
    /// Only a bound, and only for a client that reads outside the app region
    /// -- looking below it for the kernel's attributes, say. An access
    /// outside this is refused by the transport and fails the command, so a
    /// client that cannot see this value should not go looking. `None` means
    /// it was resolved by a `tock` that did not carry it.
    #[serde(default)]
    pub flash: Option<Region>,
    /// The project's `[[app]]` entries, resolved to absolute paths, so a
    /// client installing "whatever this project declares" need not find or
    /// parse `Tock.toml` itself.
    pub project_apps: Vec<PathBuf>,
}

impl Context {
    pub fn new(settings: &Settings, project_apps: Vec<PathBuf>) -> Context {
        Context {
            board: Board {
                name: settings.board.info.name.clone(),
                arch: settings.board.info.arch.clone(),
            },
            apps: settings.board.app_region(),
            placement: settings.board.layout.placement.clone(),
            // As a Region, not the bare tuple the planner takes: `[start,
            // size]` in JSON is a worse contract than named fields.
            app_ram: settings
                .board
                .app_ram()
                .map(|(start, size)| Region { start, size }),
            flash: Some(Region {
                start: settings.board.layout.flash.start.0,
                size: settings.board.layout.flash.size.0,
            }),
            project_apps,
        }
    }
}

pub fn resolve(inputs: &Inputs, files: &dyn FileSource) -> Result<Settings, Error> {
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
    let (board, board_origin) = BoardFile::load(&board_ref.value, relative_to, files)?;

    // Routes layer per capability, lowest precedence first, the way options
    // do -- so a board file saying `default = "openocd", console = "serial"`
    // survives a user config that names only `console`.
    let mut routes: BTreeMap<String, Sourced<String>> = BTreeMap::new();
    let mut route = |choice: Option<&Transport>, source: Source| {
        for (capability, name) in choice.into_iter().flat_map(Transport::routes) {
            routes.insert(
                capability,
                Sourced {
                    value: name,
                    source: source.clone(),
                },
            );
        }
    };
    route(board.transport(), Source::Board(board_origin.clone()));
    if let Some((origin, u)) = user {
        route(u.transport.as_ref(), Source::User(origin.clone()));
    }
    if let Some((_, p)) = project {
        route(p.transport.as_ref(), project_source());
    }
    if let Some((_, a)) = &alias {
        route(a.transport.as_ref(), alias_source());
    }
    // A name given outright means that transport for everything, which is
    // what someone typing `--transport serial` means. It replaces the table
    // rather than adding to it.
    for (value, source) in [
        (
            inputs.env.transport.as_ref(),
            Source::Env("TOCK_TRANSPORT".into()),
        ),
        (
            inputs.flags.transport.as_ref(),
            Source::Flag("--transport".into()),
        ),
    ] {
        if let Some(name) = value {
            routes.clear();
            routes.insert(
                DEFAULT_ROUTE.to_string(),
                Sourced {
                    value: name.clone(),
                    source,
                },
            );
        }
    }
    if routes.is_empty() {
        return Err(Error::NoTransport(board.info.name.clone()));
    }

    // Options, lowest precedence first so later layers overwrite -- and now
    // once per transport any route names, because a client may need two of
    // them and each has its own.
    let mut options: BTreeMap<String, BTreeMap<String, Sourced<toml::Value>>> = BTreeMap::new();
    let routed: Vec<String> = {
        let mut names: Vec<String> = routes.values().map(|s| s.value.clone()).collect();
        names.sort();
        names.dedup();
        names
    };
    let default = routes.get(DEFAULT_ROUTE).map(|s| s.value.clone());
    for name in routed {
        let mut resolved: BTreeMap<String, Sourced<toml::Value>> = BTreeMap::new();
        let mut layer = |table: Option<std::borrow::Cow<'_, toml::Table>>, source: Source| {
            for (k, v) in table.iter().flat_map(|t| t.iter()) {
                resolved.insert(
                    k.clone(),
                    Sourced {
                        value: v.clone(),
                        source: source.clone(),
                    },
                );
            }
        };
        layer(
            board
                .transport_options(&name)
                .map(std::borrow::Cow::Borrowed),
            Source::Board(board_origin.clone()),
        );
        if let Some((origin, u)) = user {
            layer(
                u.transports.get(&name).map(std::borrow::Cow::Borrowed),
                Source::User(origin.clone()),
            );
        }
        if let Some((_, p)) = project {
            layer(
                p.transports.get(&name).map(std::borrow::Cow::Borrowed),
                project_source(),
            );
        }
        if let Some((_, a)) = &alias {
            // An alias may carry options for more than one transport, keyed
            // the same way: a sub-table named for a routed transport is that
            // transport's, and anything else is the default route's.
            layer(
                alias_options(&a.options, &name, default.as_deref()),
                alias_source(),
            );
        }
        for over in &inputs.env.options {
            if over.applies_to(&name, default.as_deref()) {
                resolved.insert(
                    over.key.clone(),
                    Sourced {
                        value: parse_option_value(&over.value),
                        source: Source::Env(env_option_var(&over.key)),
                    },
                );
            }
        }
        for over in &inputs.flags.options {
            if over.applies_to(&name, default.as_deref()) {
                resolved.insert(
                    over.key.clone(),
                    Sourced {
                        value: parse_option_value(&over.value),
                        source: Source::Flag(format!("--opt {}", over.written())),
                    },
                );
            }
        }
        options.insert(name, resolved);
    }

    // An unqualified override means "the default one", so without a default
    // route it means nothing, and silently dropping it is the failure the
    // option schema exists to prevent.
    if default.is_none() {
        let orphan = inputs
            .flags
            .options
            .iter()
            .chain(&inputs.env.options)
            .find(|over| over.transport.is_none());
        if let Some(over) = orphan {
            return Err(Error::Invalid(format!(
                "`{}` does not say which transport it is for, and this board has no \
                 default route to assume -- write it as `<transport>.{}`",
                over.key, over.key
            )));
        }
    }

    Ok(Settings {
        board_file: Sourced {
            value: board_origin,
            source: board_ref.source,
        },
        board,
        transport: TransportContext { routes, options },
        device,
        project: project.map(|(o, _)| o.clone()),
        user: user.map(|(o, _)| o.clone()),
    })
}

/// A device alias's options for one transport.
///
/// An alias names a board and how to reach it, which may be more than one
/// way, so its options are keyed like everything else: a sub-table named
/// for a routed transport belongs to that transport, and the rest belong to
/// the default route. Matching against the routed names rather than "is it
/// a table" keeps an option whose value is genuinely a table from being
/// mistaken for a qualifier.
fn alias_options<'a>(
    options: &'a toml::Table,
    name: &str,
    default: Option<&str>,
) -> Option<std::borrow::Cow<'a, toml::Table>> {
    let qualified = options.get(name).and_then(|v| v.as_table());
    if default != Some(name) {
        return qualified.map(std::borrow::Cow::Borrowed);
    }
    // The default route's own options, with any qualifier tables removed so
    // another transport's settings do not leak into it.
    let mut plain = options.clone();
    plain.retain(|_, v| !v.is_table());
    if let Some(qualified) = qualified {
        for (k, v) in qualified {
            plain.insert(k.clone(), v.clone());
        }
    }
    Some(std::borrow::Cow::Owned(plain))
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
