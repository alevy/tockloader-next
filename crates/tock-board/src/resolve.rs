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
//! Transport options layer per key in the same order, except that there is no
//! board/transport/device equivalent of `[connection.<name>]`: flag, device
//! alias, project file, user config, board file.
//!
//! Every resolved value carries its [`Source`], so `tock config show` can
//! explain all of it.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::{Board, Error, FileSource, ProjectFile, UserConfig};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "from", content = "at", rename_all = "kebab-case")]
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
/// kebab-case, so `chip-name` is `TOCK_OPT_CHIP_NAME`.
pub fn env_option_var(key: &str) -> String {
    format!("TOCK_OPT_{}", key.to_uppercase().replace('-', "_"))
}

/// The option key an environment variable name carries, if it is one.
pub fn env_option_key(var: &str) -> Option<String> {
    var.strip_prefix("TOCK_OPT_")
        .filter(|rest| !rest.is_empty())
        .map(|rest| rest.to_lowercase().replace('_', "-"))
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
mod tests {
    use super::*;

    struct Files(Vec<(&'static str, &'static str)>);
    impl FileSource for Files {
        fn read(&self, reference: &str, _rel: Option<&str>) -> Result<(String, String), Error> {
            self.0
                .iter()
                .find(|(p, _)| *p == reference)
                .map(|(p, t)| (p.to_string(), t.to_string()))
                .ok_or_else(|| Error::Read(reference.into(), "not found".into()))
        }
    }

    fn inputs<'a>(
        flags: Layer,
        project: Option<(String, &'a ProjectFile)>,
        user: Option<(String, &'a UserConfig)>,
    ) -> Inputs<'a> {
        Inputs {
            flags,
            env: Layer::default(),
            project,
            user,
        }
    }

    #[test]
    fn board_default_transport_and_options() {
        let project = ProjectFile {
            board: Some("nrf52840dk".into()),
            ..Default::default()
        };
        let ctx = resolve(
            &inputs(Layer::default(), Some(("Tock.toml".into(), &project)), None),
            &Files(vec![]),
        )
        .unwrap();
        assert_eq!(ctx.board.board.name, "nrf52840dk");
        assert_eq!(ctx.board_file.source, Source::Project("Tock.toml".into()));
        assert_eq!(ctx.transport.name.value, "openocd");
        assert!(matches!(ctx.transport.name.source, Source::Board(_)));
        assert!(ctx.transport.options.contains_key("config"));
    }

    #[test]
    fn precedence_flag_over_alias_over_project_over_user() {
        let project = ProjectFile {
            board: Some("nrf52840dk".into()),
            transport: Some("probe-rs".into()),
            device: Some("lab".into()),
            ..Default::default()
        };
        let user = UserConfig::parse(
            "user.toml",
            r#"
            [devices.lab]
            transport = "jlink"
            serial = "123"
            [transports.jlink]
            speed = 4000
            serial = "default"
            "#,
        )
        .unwrap();
        let ctx = resolve(
            &inputs(
                Layer::default(),
                Some(("Tock.toml".into(), &project)),
                Some(("user.toml".into(), &user)),
            ),
            &Files(vec![]),
        )
        .unwrap();
        // The alias beats the project file.
        assert_eq!(ctx.transport.name.value, "jlink");
        assert_eq!(ctx.transport.name.source, Source::Device("lab".into()));
        // Board < user < alias for options.
        assert_eq!(
            ctx.transport.options["device"].source,
            Source::Board("bundled:nrf52840dk".into())
        );
        assert_eq!(
            ctx.transport.options["speed"].value.as_integer(),
            Some(4000)
        );
        assert_eq!(ctx.transport.options["serial"].value.as_str(), Some("123"));

        let flags = Layer {
            transport: Some("openocd".into()),
            options: vec![("speed".into(), "100".into())],
            ..Default::default()
        };
        let ctx = resolve(
            &inputs(
                flags,
                Some(("Tock.toml".into(), &project)),
                Some(("user.toml".into(), &user)),
            ),
            &Files(vec![]),
        )
        .unwrap();
        assert_eq!(ctx.transport.name.value, "openocd");
        assert_eq!(
            ctx.transport.options["speed"].source,
            Source::Flag("--opt speed".into())
        );
    }

    #[test]
    fn project_transport_options_beat_the_board_file() {
        let project = ProjectFile::parse(
            "Tock.toml",
            r#"
            board = "nrf52840dk"
            transport = "openocd"
            [transports.openocd]
            config = ["project.cfg"]
            speed = 9000
            [transports.probe-rs]
            chip = "not-selected"
            "#,
        )
        .unwrap();
        let ctx = resolve(
            &inputs(Layer::default(), Some(("Tock.toml".into(), &project)), None),
            &Files(vec![]),
        )
        .unwrap();
        assert_eq!(
            ctx.transport.options["config"].source,
            Source::Project("Tock.toml".into())
        );
        assert_eq!(ctx.transport.options["speed"].value.as_integer(), Some(9000));
        // A table for a transport that was not selected contributes nothing.
        assert!(!ctx.transport.options.contains_key("chip"));
    }

    #[test]
    fn alias_options_beat_project_options() {
        let project = ProjectFile::parse(
            "Tock.toml",
            "board = \"nrf52840dk\"\ntransport = \"jlink\"\ndevice = \"lab\"\n\
             [transports.jlink]\nserial = \"from-project\"\nspeed = 9000\n",
        )
        .unwrap();
        let user = UserConfig::parse(
            "user.toml",
            "[devices.lab]\nserial = \"from-alias\"\n[transports.jlink]\nspeed = 1\n",
        )
        .unwrap();
        let ctx = resolve(
            &inputs(
                Layer::default(),
                Some(("Tock.toml".into(), &project)),
                Some(("user.toml".into(), &user)),
            ),
            &Files(vec![]),
        )
        .unwrap();
        assert_eq!(
            ctx.transport.options["serial"].value.as_str(),
            Some("from-alias")
        );
        // ...and project still beats user config.
        assert_eq!(ctx.transport.options["speed"].value.as_integer(), Some(9000));
    }

    #[test]
    fn env_options_beat_project_and_lose_to_flags() {
        let project = ProjectFile::parse(
            "Tock.toml",
            "board = \"nrf52840dk\"\ntransport = \"file\"\n\
             [transports.file]\npath = \"from-project\"\n",
        )
        .unwrap();
        let env_layer = || Layer {
            options: vec![("path".into(), "from-env".into())],
            ..Default::default()
        };

        let mut i = inputs(Layer::default(), Some(("Tock.toml".into(), &project)), None);
        i.env = env_layer();
        let ctx = resolve(&i, &Files(vec![])).unwrap();
        assert_eq!(ctx.transport.options["path"].value.as_str(), Some("from-env"));
        assert_eq!(
            ctx.transport.options["path"].source,
            Source::Env("TOCK_OPT_PATH".into())
        );

        let flags = Layer {
            options: vec![("path".into(), "from-flag".into())],
            ..Default::default()
        };
        let mut i = inputs(flags, Some(("Tock.toml".into(), &project)), None);
        i.env = env_layer();
        let ctx = resolve(&i, &Files(vec![])).unwrap();
        assert_eq!(
            ctx.transport.options["path"].value.as_str(),
            Some("from-flag")
        );
    }

    #[test]
    fn extends_overrides_one_field() {
        let files = Files(vec![(
            "boards/acme.toml",
            r#"
            extends = "nrf52840dk"
            [board]
            name = "acme"
            [layout]
            apps = { start = 0x50000, size = "704K" }
            "#,
        )]);
        let flags = Layer {
            board: Some("boards/acme.toml".into()),
            ..Default::default()
        };
        let ctx = resolve(&inputs(flags, None, None), &files).unwrap();
        assert_eq!(ctx.board.board.name, "acme");
        assert_eq!(ctx.board.board.arch, "cortex-m4");
        assert_eq!(ctx.board.app_region().start, 0x50000);
        assert_eq!(ctx.board.layout.flash.page_size.0, 4096);
    }

    #[test]
    fn no_guessing() {
        let err = resolve(&inputs(Layer::default(), None, None), &Files(vec![])).unwrap_err();
        assert!(matches!(err, Error::NoBoard));
        let flags = Layer {
            board: Some("nrf52".into()),
            ..Default::default()
        };
        let err = resolve(&inputs(flags, None, None), &Files(vec![])).unwrap_err();
        assert!(matches!(err, Error::UnknownBoard(_)));
    }

    #[test]
    fn layout_is_validated() {
        let bad = r#"
            [board]
            name = "x"
            arch = "cortex-m4"
            [layout]
            flash = { start = 0, size = "1M", page-size = "4K" }
            kernel = { start = 0, size = "256K" }
            apps = { start = 0x20000, size = "128K" }
        "#;
        let err = Board::parse("x.toml", bad).unwrap_err().to_string();
        assert!(err.contains("overlaps layout.kernel"), "{err}");
    }
}
