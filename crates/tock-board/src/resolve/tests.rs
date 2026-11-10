//! Tests for resolution: which layer wins, and why it says so.

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
    assert_eq!(ctx.board.info.name, "nrf52840dk");
    assert_eq!(ctx.board_file.source, Source::Project("Tock.toml".into()));
    assert_eq!(ctx.transport.name.value, "openocd");
    assert!(matches!(ctx.transport.name.source, Source::Board(_)));
    // Both halves come from the board file: the chip, which is the same for
    // everyone, and a probe default, which is only right until someone
    // plugs in a different one.
    for key in ["target", "probe", "page_size"] {
        let option = ctx.transport.options.get(key).expect(key);
        assert!(matches!(option.source, Source::Board(_)), "{key}");
    }
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
    // Board < user < alias for options. `chip` is supplied only by the board
    // file, so its source proves the board layer reached the resolved set.
    assert_eq!(
        ctx.transport.options["chip"].source,
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
    assert_eq!(
        ctx.transport.options["speed"].value.as_integer(),
        Some(9000)
    );
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
    assert_eq!(
        ctx.transport.options["speed"].value.as_integer(),
        Some(9000)
    );
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
    assert_eq!(
        ctx.transport.options["path"].value.as_str(),
        Some("from-env")
    );
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
    assert_eq!(ctx.board.info.name, "acme");
    assert_eq!(ctx.board.info.arch, "cortex-m4");
    assert_eq!(ctx.board.app_region().start, 0x50000);
    // Inherited from the base board, not restated by the override.
    assert_eq!(ctx.board.layout.flash.size.0, 1024 * 1024);
}

#[test]
fn the_exported_document_round_trips_and_carries_no_transport_detail() {
    let project = ProjectFile::parse(
        "Tock.toml",
        "board = \"nrf52840dk\"\ntransport = \"openocd\"\n\
         [transports.openocd]\nconfig = [\"secret.cfg\"]\n",
    )
    .unwrap();
    let ctx = resolve(
        &inputs(Layer::default(), Some(("Tock.toml".into(), &project)), None),
        &Files(vec![]),
    )
    .unwrap();
    let exported = Context::new(&ctx, vec![PathBuf::from("/repo/blink.tab")]);
    let json = serde_json::to_string(&exported).unwrap();

    // What a client plans with survives the trip.
    let back: Context = serde_json::from_str(&json).unwrap();
    assert_eq!(back.board.name, "nrf52840dk");
    assert_eq!(back.board.arch, "cortex-m4");
    assert_eq!(back.apps, ctx.board.app_region());
    assert_eq!(back.project_apps, vec![PathBuf::from("/repo/blink.tab")]);

    // How we reached the device does not.
    assert!(!json.contains("openocd"), "{json}");
    assert!(!json.contains("secret.cfg"), "{json}");
    assert!(!json.contains("Tock.toml"), "{json}");
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
fn env_option_names_round_trip_without_rewriting_separators() {
    // Keys are snake_case, so the mapping is just case: no separator is
    // rewritten, and a multi-word key is reachable from the environment.
    assert_eq!(env_option_var("chip_name"), "TOCK_OPT_CHIP_NAME");
    assert_eq!(
        env_option_key("TOCK_OPT_CHIP_NAME").as_deref(),
        Some("chip_name")
    );
    assert_eq!(env_option_key("TOCK_OPT_PATH").as_deref(), Some("path"));
    assert_eq!(env_option_key("TOCK_OPT_"), None);
    assert_eq!(env_option_key("TOCK_BOARD"), None);
}

#[test]
fn board_files_use_snake_case_keys_and_values() {
    let files = Files(vec![(
        "boards/snake.toml",
        r#"
        [board]
        name = "snake"
        arch = "cortex-m4"
        transport = "probe-rs"
        [layout]
        flash = { start = 0, size = "1M" }
        kernel = { start = 0, size = "256K" }
        apps = { start = 0x40000, size = "768K" }
        app_ram = { start = 0x20000000, size = "256K" }
        [layout.placement]
        order = "size_descending"
        size = "power_of_two"
        [transports.probe-rs]
        chip = "X"
        "#,
    )]);
    let flags = Layer {
        board: Some("boards/snake.toml".into()),
        ..Default::default()
    };
    let ctx = resolve(&inputs(flags, None, None), &files).unwrap();
    assert!(ctx.board.layout.app_ram.is_some());
    // Transport names stay as they are: `probe-rs` names an executable.
    assert_eq!(ctx.transport.name.value, "probe-rs");
    assert_eq!(ctx.transport.options["chip"].value.as_str(), Some("X"));
}

#[test]
fn board_files_no_longer_carry_a_page_size() {
    let stale = r#"
        [board]
        name = "x"
        arch = "cortex-m4"
        [layout]
        flash = { start = 0, size = "1M", page_size = "4K" }
        kernel = { start = 0, size = "256K" }
        apps = { start = 0x40000, size = "768K" }
    "#;
    // Loud, not ignored: someone still believes that number means something.
    let err = BoardFile::parse("x.toml", stale).unwrap_err().to_string();
    assert!(err.contains("page_size"), "{err}");
}

#[test]
fn an_unaligned_app_region_is_the_kernels_business() {
    let odd = r#"
        [board]
        name = "x"
        arch = "cortex-m4"
        [layout]
        flash = { start = 0, size = "1M" }
        kernel = { start = 0, size = "256K" }
        apps = { start = 0x40010, size = "700K" }
    "#;
    assert!(BoardFile::parse("x.toml", odd).is_ok());
}

#[test]
fn layout_is_validated() {
    let bad = r#"
        [board]
        name = "x"
        arch = "cortex-m4"
        [layout]
        flash = { start = 0, size = "1M" }
        kernel = { start = 0, size = "256K" }
        apps = { start = 0x20000, size = "128K" }
    "#;
    let err = BoardFile::parse("x.toml", bad).unwrap_err().to_string();
    assert!(err.contains("overlaps layout.kernel"), "{err}");
}
