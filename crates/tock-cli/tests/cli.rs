//! End-to-end tests of the `tock` binary against flash image files.
//!
//! The `python_*` tests compare against Python tockloader byte for byte. They
//! run only when `TOCKLOADER_PY` points at a `tockloader` executable.

use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tock_image::Region;
use tock_image::kernel::Attributes;

const FLASH_SIZE: usize = 0x10_0000; // nRF52840: 1 MiB at 0
/// Where `boards/nrf52840dk.toml` says apps begin, and so where a kernel
/// would leave its attribute table.
const APPS_START: usize = 0x4_0000;

/// What a real nrf52840dk kernel reports. Taken from a kernel built out of
/// the Tock tree; the parser's own tests check these bytes exactly.
fn kernel_attributes() -> Attributes {
    Attributes {
        version: tock_image::kernel::VERSION,
        app_ram: Some(Region {
            start: 0x2000_8704,
            size: 227_580,
        }),
        kernel_binary: Some(Region {
            start: 0,
            size: 172_032,
        }),
        kernel_version: Some(tock_image::kernel::Version {
            major: 2,
            minor: 4,
            patch: 0,
            prerelease: 1,
        }),
        other: Vec::new(),
    }
}

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }
    fn path(&self, f: &str) -> PathBuf {
        self.0.join(f)
    }
    fn erased(&self, f: &str) -> PathBuf {
        let p = self.path(f);
        std::fs::write(&p, vec![0xFF; FLASH_SIZE]).unwrap();
        p
    }

    /// An erased image with a kernel attribute table ending where the app
    /// region begins, which is where a real kernel leaves one.
    fn with_kernel(&self, f: &str, attrs: &Attributes) -> PathBuf {
        let p = self.path(f);
        let mut image = vec![0xFF; FLASH_SIZE];
        let table = attrs.to_bytes();
        image[APPS_START - table.len()..APPS_START].copy_from_slice(&table);
        std::fs::write(&p, image).unwrap();
        p
    }
    /// A TAB like elf2tab's, for `cortex-m4`, optionally built for a fixed
    /// flash address.
    fn tab(&self, name: &str, total: u32, fixed_flash: Option<u32>) -> PathBuf {
        self.tab_ram(name, total, fixed_flash, 0x2000_8000)
    }

    /// The same, with a say in the fixed *RAM* address -- which is what the
    /// kernel's attribute table has an opinion about.
    fn tab_ram(&self, name: &str, total: u32, fixed_flash: Option<u32>, ram: u32) -> PathBuf {
        let tbf = tbf::testing::app(name, total, total / 2, fixed_flash.map(|f| (ram, f)));
        let meta = format!("tab-version = 1\nname = \"{name}\"\n");
        let p = self.path(&format!("{name}.tab"));
        // Content only, no filler: Python re-serializes footers, so filler
        // would not survive its round trip (see docs/spike/m0-findings.md).
        std::fs::write(
            &p,
            tbf::Tab::build(&meta, &[("cortex-m4.tbf", &tbf.content_bytes())]),
        )
        .unwrap();
        p
    }
}

/// Runs in `dir`, seeing only `dir`'s own config. Pinning `TOCK_PROJECT` keeps
/// the suite hermetic: `CARGO_TARGET_TMPDIR` lives inside the repo, so without
/// it every test inherits whatever `Tock.toml` happens to sit above the
/// checkout. Tests that want the walk up itself set their own environment.
fn tock(dir: &Dir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tock"))
        .args(args)
        .current_dir(&dir.0)
        .env("TOCK_CONFIG", dir.path("no-user-config.toml"))
        .env("TOCK_PROJECT", dir.path("Tock.toml"))
        .output()
        .unwrap()
}

/// Like [`tock`], but run from `sub` inside the project, to prove that a
/// relative path in `Tock.toml` does not depend on the working directory.
fn tock_in(dir: &Dir, sub: &str, args: &[&str]) -> Output {
    let cwd = dir.0.join(sub);
    std::fs::create_dir_all(&cwd).unwrap();
    Command::new(env!("CARGO_BIN_EXE_tock"))
        .args(args)
        .current_dir(&cwd)
        .env("TOCK_CONFIG", dir.path("no-user-config.toml"))
        .env("TOCK_PROJECT", dir.path("Tock.toml"))
        .output()
        .unwrap()
}

fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn on_file<'a>(image: &'a str, rest: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec!["--board", "nrf52840dk", "--transport", "file", "--opt"];
    v.push(image);
    v.extend_from_slice(rest);
    v
}

fn list_json(dir: &Dir, opt: &str) -> Vec<(String, u64, u64)> {
    let out = ok(tock(dir, &on_file(opt, &["--format", "json", "list"])));
    let v: Value = serde_json::from_str(&out).unwrap();
    v["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["name"].as_str().unwrap().to_string(),
                a["address"].as_u64().unwrap(),
                a["size"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn tock_project_env_pins_the_project_file() {
    let d = Dir::new("tock_project_pin");
    std::fs::create_dir_all(d.path("elsewhere")).unwrap();
    // A board no other Tock.toml in or above this tree names, so a reported
    // board of `raspberry_pi_pico_2` can only have come from the pinned file.
    std::fs::write(
        d.path("elsewhere/Project.toml"),
        "board = \"raspberry_pi_pico_2\"\ntransport = \"file\"\n",
    )
    .unwrap();
    std::fs::write(
        d.path("Tock.toml"),
        "board = \"nrf52840dk\"\ntransport = \"file\"\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tock"))
        .args(["config", "show"])
        .current_dir(&d.0)
        .env("TOCK_CONFIG", d.path("no-user-config.toml"))
        .env("TOCK_PROJECT", d.path("elsewhere/Project.toml"))
        .output()
        .unwrap();
    let shown = ok(out);
    assert!(shown.contains("board      raspberry_pi_pico_2"), "{shown}");
    assert!(shown.contains("elsewhere/Project.toml"), "{shown}");
}

#[test]
fn tock_project_env_set_to_a_nonfile_means_no_project() {
    let d = Dir::new("tock_project_none");
    std::fs::write(
        d.path("Tock.toml"),
        "board = \"nrf52840dk\"\ntransport = \"file\"\n",
    )
    .unwrap();
    // A Tock.toml is right there, and an empty TOCK_PROJECT still suppresses it.
    let out = Command::new(env!("CARGO_BIN_EXE_tock"))
        .args(["list"])
        .current_dir(&d.0)
        .env("TOCK_CONFIG", d.path("no-user-config.toml"))
        .env("TOCK_PROJECT", "")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("no board selected"), "{err}");
}

#[test]
fn install_list_uninstall() {
    let d = Dir::new("install_list_uninstall");
    d.erased("flash.bin");
    let (a, b) = (d.tab("a", 5000, None), d.tab("b", 30000, None));
    let opt = "path=flash.bin";
    ok(tock(
        &d,
        &on_file(opt, &["install", a.to_str().unwrap(), b.to_str().unwrap()]),
    ));
    assert_eq!(
        list_json(&d, opt),
        vec![("b".into(), 0x40000, 32768), ("a".into(), 0x48000, 8192)]
    );
    ok(tock(&d, &on_file(opt, &["uninstall", "b"])));
    assert_eq!(list_json(&d, opt), vec![("a".into(), 0x48000, 8192)]);
}

#[test]
fn dry_run_writes_nothing_and_reports_the_plan() {
    let d = Dir::new("dry_run");
    d.erased("flash.bin");
    let before = std::fs::read(d.path("flash.bin")).unwrap();
    let a = d.tab("a", 5000, None);
    let out = ok(tock(
        &d,
        &on_file(
            "path=flash.bin",
            &[
                "--format",
                "json",
                "install",
                "--dry-run",
                a.to_str().unwrap(),
            ],
        ),
    ));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["plan"]["actions"][0]["action"], "install");
    // Writes are runs of bytes now; reporting bare addresses as "pages" names
    // the thing this tool no longer has.
    assert!(v["pages_written"].is_null(), "{v}");
    assert!(v["writes"][0]["bytes"].as_u64().unwrap() > 0, "{v}");
    assert_eq!(v["plan"]["actions"][0]["address"], 0x40000);
    assert_eq!(std::fs::read(d.path("flash.bin")).unwrap(), before);
}

#[test]
fn project_file_drives_everything() {
    let d = Dir::new("project");
    d.erased("flash.bin");
    d.tab("svc", 4000, None);
    d.tab("app", 9000, None);
    std::fs::write(
        d.path("Tock.toml"),
        "board = \"nrf52840dk\"\ntransport = \"file\"\n\
         [transports.file]\npath = \"flash.bin\"\n\
         [[app]]\ntab = \"svc.tab\"\n[[app]]\ntab = \"app.tab\"\n",
    )
    .unwrap();
    // One command, no flags at all.
    ok(tock(&d, &["install"]));
    let names: Vec<String> = list_json(&d, "path=flash.bin")
        .into_iter()
        .map(|a| a.0)
        .collect();
    assert_eq!(names, vec!["app", "svc"]);

    let shown = ok(tock(&d, &["config", "show"]));
    assert!(shown.contains("transport  file"), "{shown}");
    assert!(shown.contains("from project"), "{shown}");
    assert!(
        shown
            .lines()
            .any(|l| l.trim_start().starts_with("path =") && l.contains("project")),
        "{shown}"
    );
}

#[test]
fn errors_say_what_to_run() {
    let d = Dir::new("errors");
    let out = tock(&d, &["list"]);
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("no board selected"), "{err}");
    assert!(
        err.contains("try: tock --board <nrf52840dk|raspberry_pi_pico_2|path/to/board.toml>"),
        "{err}"
    );

    let out = tock(
        &d,
        &["--board", "nrf52840dk", "--transport", "file", "list"],
    );
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("try: add --opt path=flash.bin --opt create=true"),
        "{err}"
    );
    assert!(
        err.contains("[transports.file]") && err.contains("Tock.toml"),
        "{err}"
    );

    let out = tock(
        &d,
        &["--board", "nrf52840dk", "--transport", "nosuch", "list"],
    );
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("tock-transport-nosuch"), "{err}");
    // Nothing is built in any more, so nothing may claim to be.
    assert!(!err.contains("built-in"), "{err}");
}

#[test]
fn needs_repack_suggests_flag() {
    let d = Dir::new("repack");
    d.erased("flash.bin");
    let opt = "path=flash.bin";
    // Fill 768K: 512K + 128K + 64K + 64K, then free the first 64K app.
    for (n, s) in [
        ("big", 400_000),
        ("m", 100_000),
        ("s1", 60_000),
        ("s2", 60_000),
    ] {
        let t = d.tab(n, s, None);
        ok(tock(&d, &on_file(opt, &["install", t.to_str().unwrap()])));
    }
    ok(tock(&d, &on_file(opt, &["uninstall", "s1"])));
    let t = d.tab("m2", 100_000, None);
    let out = tock(&d, &on_file(opt, &["install", t.to_str().unwrap()]));
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("does not fit"), "{err}");
}

#[test]
fn the_file_transport_is_a_standalone_executable() {
    use tock_proto::{Capability, Connection, Request, Response, StreamChannel};
    let d = Dir::new("standalone");
    d.erased("flash.bin");

    // Driven directly, with no `tock` in the picture at all: it is an ordinary
    // transport executable, not something the core knows about.
    let mut child = Command::new(env!("CARGO_BIN_EXE_tock-transport-file"))
        .current_dir(&d.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ch = StreamChannel {
        reader: child.stdout.take().unwrap(),
        writer: child.stdin.take().unwrap(),
    };
    let (hello, _) = ch
        .call(
            Request::Hello {
                protocol: 0,
                client: "test".into(),
            },
            vec![],
        )
        .unwrap();
    match hello {
        Response::Hello {
            transport,
            capabilities,
            ..
        } => {
            assert_eq!(transport, "file");
            assert!(capabilities.contains(&Capability::MemoryRead));
            assert!(capabilities.contains(&Capability::MemoryWrite));
        }
        other => panic!("{other:?}"),
    }
    ch.call(Request::Close, vec![]).unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn broker_serves_the_device() {
    use tock_proto::{Connection, Request, Response, StreamChannel};
    let d = Dir::new("broker");
    d.erased("flash.bin");
    let a = d.tab("a", 5000, None);
    ok(tock(
        &d,
        &on_file("path=flash.bin", &["install", a.to_str().unwrap()]),
    ));

    let mut child = Command::new(env!("CARGO_BIN_EXE_tock"))
        .args(on_file("path=flash.bin", &["broker"]))
        .current_dir(&d.0)
        .env("TOCK_CONFIG", d.path("none.toml"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ch = StreamChannel {
        reader: child.stdout.take().unwrap(),
        writer: child.stdin.take().unwrap(),
    };
    let (hello, _) = ch
        .call(
            Request::Hello {
                protocol: 0,
                client: "test".into(),
            },
            vec![],
        )
        .unwrap();
    assert!(matches!(hello, Response::Hello { ref transport, .. } if transport == "file"));
    let (_, data) = ch
        .call(
            Request::Read {
                address: 0x40000,
                len: 16,
            },
            vec![],
        )
        .unwrap();
    let header = tbf::Lengths::parse(&data).unwrap();
    assert_eq!(header.total_size, 8192);
    ch.call(Request::Close, vec![]).unwrap();
    assert!(child.wait().unwrap().success());
}

// ---------------------------------------------------------------------------
// Comparison with Python tockloader

fn python() -> Option<String> {
    std::env::var("TOCKLOADER_PY").ok()
}

fn py(dir: &Dir, args: &[&str]) {
    let out = Command::new(python().unwrap())
        .args(args)
        .args(["--flash-file", "py.bin", "--board", "nrf52dk"])
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Compare images, ignoring end-of-list markers, current and stale (Python's
/// flash-file backend writes one zero byte; tock writes 8 erased bytes).
fn assert_same(d: &Dir, ends: &[usize]) {
    let mut a = std::fs::read(d.path("py.bin")).unwrap();
    let mut b = std::fs::read(d.path("rs.bin")).unwrap();
    for &end in ends {
        a[end..end + 8].fill(0xFF);
        b[end..end + 8].fill(0xFF);
    }
    let first = a.iter().zip(&b).position(|(x, y)| x != y);
    assert_eq!(
        first,
        None,
        "images differ first at {:#x}",
        first.unwrap_or(0)
    );
}

fn end_of_list(d: &Dir) -> usize {
    let out = ok(tock(
        d,
        &on_file("path=rs.bin", &["--format", "json", "list"]),
    ));
    serde_json::from_str::<Value>(&out).unwrap()["end"]
        .as_u64()
        .unwrap() as usize
}

#[test]
fn python_same_bytes_for_install_uninstall_and_repack() {
    if python().is_none() {
        eprintln!("skipped: set TOCKLOADER_PY");
        return;
    }
    let d = Dir::new("python_compare");
    d.erased("py.bin");
    d.erased("rs.bin");
    let tabs: Vec<String> = [("a", 5000), ("b", 30000), ("c", 2000)]
        .iter()
        .map(|(n, s)| d.tab(n, *s, None).display().to_string())
        .collect();
    let tabs: Vec<&str> = tabs.iter().map(String::as_str).collect();
    let rs = |rest: &[&str]| ok(tock(&d, &on_file("path=rs.bin", rest)));
    let mut ends = vec![];

    // Install three apps at once.
    py(&d, &[&["install"], &tabs[..]].concat());
    rs(&[&["install"], &tabs[..]].concat());
    ends.push(end_of_list(&d));
    assert_same(&d, &ends);

    // Uninstall the middle one: both replace its header with padding.
    py(&d, &["uninstall", "a"]);
    rs(&["uninstall", "a"]);
    ends.push(end_of_list(&d));
    assert_same(&d, &ends);

    // A bigger app: Python re-sorts and moves apps; tock does the same only
    // when allowed to.
    let big = d.tab("d", 60000, None).display().to_string();
    py(&d, &["install", &big]);
    rs(&["install", "--allow-repack", &big]);
    ends.push(end_of_list(&d));
    assert_same(&d, &ends);
}

#[test]
fn python_same_placement_for_fixed_address_apps() {
    if python().is_none() {
        eprintln!("skipped: set TOCKLOADER_PY");
        return;
    }
    let d = Dir::new("python_fixed");
    d.erased("py.bin");
    d.erased("rs.bin");
    let f1 = d.tab("f1", 4096, Some(0x40080)).display().to_string();
    let f2 = d.tab("f2", 8192, Some(0x48080)).display().to_string();
    py(&d, &["install", &f1, &f2]);
    ok(tock(&d, &on_file("path=rs.bin", &["install", &f1, &f2])));
    let rs_apps = list_json(&d, "path=rs.bin");
    std::fs::copy(d.path("py.bin"), d.path("py-view.bin")).unwrap();
    let py_apps = list_json(&d, "path=py-view.bin");
    assert_eq!(rs_apps, py_apps);
    assert_eq!(
        rs_apps,
        vec![("f1".into(), 0x40000, 4096), ("f2".into(), 0x48000, 8192)]
    );
}

#[test]
fn project_paths_resolve_against_the_project_file() {
    let d = Dir::new("project_subdir");
    d.erased("flash.bin");
    d.tab("app", 9000, None);
    std::fs::write(
        d.path("Tock.toml"),
        "board = \"nrf52840dk\"\ntransport = \"file\"\n\
         [transports.file]\npath = \"flash.bin\"\n\
         [[app]]\ntab = \"app.tab\"\n",
    )
    .unwrap();
    // Two directories down, no flags: still the image next to Tock.toml.
    ok(tock_in(&d, "src/deep", &["install"]));
    assert_eq!(
        list_json(&d, "path=flash.bin")
            .into_iter()
            .map(|a| a.0)
            .collect::<Vec<_>>(),
        vec!["app"]
    );
    ok(tock_in(&d, "src/deep", &["uninstall", "app"]));
    assert!(list_json(&d, "path=flash.bin").is_empty());
}

#[test]
fn walking_up_to_the_project_file_still_works() {
    let d = Dir::new("project_walkup");
    d.erased("flash.bin");
    let app = d.tab("app", 9000, None);
    std::fs::write(
        d.path("Tock.toml"),
        "board = \"nrf52840dk\"\ntransport = \"file\"\n\
         [transports.file]\npath = \"flash.bin\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(d.path("src/deep")).unwrap();
    // No TOCK_PROJECT: discovery has to walk up, and the nearest Tock.toml
    // wins over any that sit above the checkout.
    let out = Command::new(env!("CARGO_BIN_EXE_tock"))
        .args(["install", app.to_str().unwrap()])
        .current_dir(d.path("src/deep"))
        .env("TOCK_CONFIG", d.path("no-user-config.toml"))
        .env_remove("TOCK_PROJECT")
        .output()
        .unwrap();
    ok(out);
    assert_eq!(
        list_json(&d, "path=flash.bin")
            .into_iter()
            .map(|a| a.0)
            .collect::<Vec<_>>(),
        vec!["app"]
    );
}

#[test]
fn flag_paths_resolve_against_the_current_directory() {
    let d = Dir::new("flag_cwd");
    let app = d.tab("app", 9000, None);
    std::fs::write(
        d.path("Tock.toml"),
        "board = \"nrf52840dk\"\ntransport = \"file\"\n\
         [transports.file]\npath = \"flash.bin\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(d.path("sub")).unwrap();
    std::fs::write(d.path("sub/other.bin"), vec![0xFFu8; FLASH_SIZE]).unwrap();
    ok(tock_in(
        &d,
        "sub",
        &["--opt", "path=other.bin", "install", app.to_str().unwrap()],
    ));
    // The flag's base is the subdirectory, and Tock.toml's image was not touched.
    assert!(d.path("sub/other.bin").is_file());
    assert!(!d.path("flash.bin").exists());
}

#[test]
fn user_config_paths_resolve_against_the_user_config() {
    let d = Dir::new("user_cfg_path");
    std::fs::create_dir_all(d.path("cfg/images")).unwrap();
    std::fs::write(d.path("cfg/images/dk.bin"), vec![0xFFu8; FLASH_SIZE]).unwrap();
    std::fs::write(
        d.path("cfg/config.toml"),
        "[transports.file]\npath = \"images/dk.bin\"\n",
    )
    .unwrap();
    let app = d.tab("app", 9000, None);
    let out = Command::new(env!("CARGO_BIN_EXE_tock"))
        .args([
            "--board",
            "nrf52840dk",
            "--transport",
            "file",
            "install",
            app.to_str().unwrap(),
        ])
        .current_dir(&d.0)
        .env("TOCK_CONFIG", d.path("cfg/config.toml"))
        .env("TOCK_PROJECT", d.path("Tock.toml"))
        .output()
        .unwrap();
    // Succeeds only because the path resolved against the config file; `create`
    // is false, so resolving against the current directory would have errored.
    ok(out);
    assert!(!d.path("images").exists());
}

#[test]
fn env_sets_transport_options() {
    let d = Dir::new("env_opt");
    d.erased("other.bin");
    let app = d.tab("app", 9000, None);
    std::fs::write(
        d.path("Tock.toml"),
        "board = \"nrf52840dk\"\ntransport = \"file\"\n\
         [transports.file]\npath = \"flash.bin\"\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tock"))
        .args(["install", app.to_str().unwrap()])
        .current_dir(&d.0)
        .env("TOCK_CONFIG", d.path("no-user-config.toml"))
        .env("TOCK_PROJECT", d.path("Tock.toml"))
        .env("TOCK_OPT_PATH", "other.bin")
        .output()
        .unwrap();
    ok(out);
    assert!(!d.path("flash.bin").exists());
    assert_eq!(
        list_json(&d, "path=other.bin")
            .into_iter()
            .map(|a| a.0)
            .collect::<Vec<_>>(),
        vec!["app"]
    );
}

#[cfg(unix)]
#[test]
fn a_non_utf8_environment_variable_does_not_break_the_tool() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let d = Dir::new("bad_env");
    d.erased("flash.bin");
    std::fs::write(
        d.path("Tock.toml"),
        "board = \"nrf52840dk\"\ntransport = \"file\"\n\
         [transports.file]\npath = \"flash.bin\"\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tock"))
        .args(["list"])
        .current_dir(&d.0)
        .env("TOCK_CONFIG", d.path("no-user-config.toml"))
        .env("TOCK_PROJECT", d.path("Tock.toml"))
        .env("SOME_OTHER_TOOL", OsString::from_vec(vec![0xff, 0xfe]))
        .output()
        .unwrap();
    // Scanning for TOCK_OPT_* must not care that some unrelated variable in
    // the environment is not valid Unicode.
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "{err}");
    ok(out);
}

#[test]
fn an_access_outside_flash_is_refused_by_the_transport() {
    use tock_proto::{Connection, Request, Response, StreamChannel};
    let d = Dir::new("broker_bounds");
    d.erased("flash.bin");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tock"))
        .args(on_file("path=flash.bin", &["broker"]))
        .current_dir(&d.0)
        .env("TOCK_CONFIG", d.path("none.toml"))
        .env("TOCK_PROJECT", d.path("Tock.toml"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ch = StreamChannel {
        reader: child.stdout.take().unwrap(),
        writer: child.stdin.take().unwrap(),
    };
    // nrf52840dk flash is 1 MiB at 0. The broker relays this; the transport
    // refuses it. `tock` used to check here as well, which hid the fact that
    // one of the two transports had no check at all -- and which protected
    // only the path that happens to go through `tock`, when a transport is an
    // executable anyone can spawn.
    let (r, _) = ch
        .call(
            Request::Read {
                address: 0x20_0000,
                len: 16,
            },
            vec![],
        )
        .unwrap();
    match r {
        Response::Error { message, .. } => assert!(message.contains("outside flash"), "{message}"),
        other => panic!("{other:?}"),
    }
    // A sub-page write is no longer anyone's business to refuse.
    let (r, _) = ch
        .call(Request::Write { address: 0x40000 }, vec![0u8; 8])
        .unwrap();
    assert!(matches!(r, Response::Ok), "{r:?}");
    ch.call(Request::Close, vec![]).unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn a_transport_that_cannot_start_still_says_how_to_fix_it() {
    // openocd is not installed at that path, so the transport cannot start at
    // all. That is fatal -- there is no session and never will be -- but the
    // user still needs the sentence telling them what to do about it.
    let d = Dir::new("openocd_no_binary");
    let out = tock(
        &d,
        &[
            "--board",
            "nrf52840dk",
            "--transport",
            "openocd",
            "--opt",
            "command=/nonexistent/openocd",
            "--opt",
            "page_size=4096",
            "list",
        ],
    );
    assert_eq!(out.status.code(), Some(5));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("cannot start `/nonexistent/openocd`"), "{err}");
    assert!(err.contains("try: install openocd"), "{err}");
}

#[test]
fn a_subcommand_is_handed_a_connection_on_fd_3() {
    // Written in Python on purpose: a subcommand should need nothing of ours
    // but the fd and the frame layout.
    let d = Dir::new("fd_handoff");
    d.erased("flash.bin");
    std::fs::write(
        d.path("Tock.toml"),
        "board = \"nrf52840dk\"\ntransport = \"file\"\n\
         [transports.file]\npath = \"flash.bin\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(d.path("bin")).unwrap();
    std::fs::write(
        d.path("bin/tock-fdtest"),
        r#"#!/usr/bin/env python3
import os, json, struct
f = os.fdopen(3, 'r+b', buffering=0)
def call(msg):
    j = json.dumps(msg).encode()
    f.write(struct.pack('<I', len(j)) + j + struct.pack('<I', 0))
    resp = json.loads(f.read(struct.unpack('<I', f.read(4))[0]))
    f.read(struct.unpack('<I', f.read(4))[0])
    return resp
hello = call({"op": "hello", "protocol": 0, "client": "fdtest"})
print("transport:", hello["transport"])
print("caps:", ",".join(sorted(hello["capabilities"])))
data = call({"op": "read", "address": 262144, "len": 8})
print("read:", data["result"])
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(d.path("bin/tock-fdtest"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(d.path("bin/tock-fdtest"), perms).unwrap();

    let path_env = format!(
        "{}:{}",
        d.path("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_tock"))
        .args(["fdtest"])
        .current_dir(&d.0)
        .env("PATH", path_env)
        .env("TOCK_CONFIG", d.path("no-user-config.toml"))
        .env("TOCK_PROJECT", d.path("Tock.toml"))
        .output()
        .unwrap();
    let stdout = ok(out);
    assert!(stdout.contains("transport: file"), "{stdout}");
    assert!(stdout.contains("memory_read"), "{stdout}");
    assert!(stdout.contains("read: data"), "{stdout}");
}

/// `tock` parses global flags and a subcommand name. It does not parse the
/// subcommand's arguments -- not even `--help`, which is the one a built-in
/// would be most tempted to answer itself. For a `tock-tickv` we have never
/// seen, answering would mean describing flags we cannot know.
#[test]
fn a_subcommand_owns_its_own_arguments() {
    // No board, no transport: parsing arguments never touches a device, so
    // help has to work in a directory where nothing else would.
    let d = Dir::new("subcommand-args");

    // Its usage line, its flags -- not `tock`'s.
    let help = ok(tock(&d, &["install", "--help"]));
    assert!(help.contains("Usage: tock-install"), "{help}");
    assert!(help.contains("--allow-repack"), "{help}");
    assert!(!help.contains("--trace-protocol"), "{help}");

    // Including the subcommand that takes no arguments at all: it still has
    // to answer, rather than ignore what it was given.
    let help = ok(tock(&d, &["list", "--help"]));
    assert!(help.contains("Usage: tock-list"), "{help}");
    let out = tock(&d, &["list", "--oops"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--oops"));

    // And `tock --help` lists them among the commands rather than in a
    // footnote, without having compiled them in. With no PATH at all, to pin
    // down that we look beside `tock` first: that is the case that matters,
    // since it is how a cargo-installed `tock` and the subcommands installed
    // alongside it find each other.
    let help = ok(Command::new(env!("CARGO_BIN_EXE_tock"))
        .args(["--help"])
        .current_dir(&d.0)
        .env_remove("PATH")
        .env("TOCK_CONFIG", d.path("no-user-config.toml"))
        .output()
        .unwrap());
    let commands = help.split("Options:").next().unwrap();
    for name in ["install", "list", "uninstall", "broker", "context"] {
        assert!(commands.contains(name), "{name} missing from:\n{help}");
    }
    // A transport is spoken to over a pipe, not typed.
    assert!(!help.contains("transport-file"), "{help}");

    // Registering a found name must not collide with one clap generates
    // itself, or an unlucky binary on PATH takes the whole tool down.
    let bin = d.path("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["tock-help", "tock-context"] {
        let p = bin.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(&p, PermissionsExt::from_mode(0o755)).unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_tock"))
        .args(["--help"])
        .current_dir(&d.0)
        .env("PATH", &bin)
        .env("TOCK_CONFIG", d.path("no-user-config.toml"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `--format json` has to reach the error path too, and it has to reach it
/// the same way whether `tock` answers the command or a subcommand does.
/// The two used to be separate code: `tock` read its own flag, a subcommand
/// read `TOCK_FORMAT`, and a subcommand's failures came out as prose.
#[test]
fn an_error_is_json_whoever_produced_it() {
    let d = Dir::new("json_errors");
    // `context` is ours; `list` is tock-list, reached through the handoff.
    for command in [
        vec!["--format", "json", "context"],
        vec!["--format", "json", "list"],
    ] {
        let out = tock(&d, &command);
        assert!(!out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        let value: Value = serde_json::from_str(text.trim())
            .unwrap_or_else(|e| panic!("{command:?} printed {text:?}: {e}"));
        assert_eq!(value["code"], 3, "{command:?}");
        assert_eq!(value["error"], "no board selected", "{command:?}");
        assert!(value["hints"].as_array().is_some_and(|h| !h.is_empty()));
    }
}

// --- kernel attributes ---------------------------------------------------

/// The kernel's own figure for app RAM replaces the board file's, and the
/// output says which it used. Every board file in this repo declares the
/// whole of RAM; the kernel has already taken its data and bss out of the
/// bottom, and how much it took is a fact about that build.
#[test]
fn the_kernel_corrects_the_board_files_app_ram() {
    let d = Dir::new("kernel-app-ram");
    let image = d.with_kernel("flash.bin", &kernel_attributes());
    let opt = format!("path={}", image.display());
    let out = ok(tock(&d, &on_file(&opt, &["--format", "json", "list"])));
    let v: Value = serde_json::from_str(&out).unwrap();

    assert_eq!(v["app_ram"]["from"], "kernel");
    assert_eq!(v["app_ram"]["start"], 0x2000_8704u64);
    assert_eq!(v["app_ram"]["size"], 227_580u64);
    assert_eq!(v["kernel"]["version"], "2.4.0-dev");
    assert_eq!(v["kernel"]["binary"]["size"], 172_032u64);
}

/// A board with no kernel on it must behave exactly as it did before any of
/// this existed: the board file's figure, no warning, no failure. Every
/// other test in this file runs against an erased image, so this is the one
/// that stops a missing table becoming fatal.
#[test]
fn a_board_with_no_kernel_is_unchanged() {
    let d = Dir::new("kernel-absent");
    let image = d.erased("flash.bin");
    let opt = format!("path={}", image.display());
    let out = tock(&d, &on_file(&opt, &["--format", "json", "list"]));
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "",
        "an absent table must be silent"
    );
    let v: Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(v["app_ram"]["from"], "board file");
    // What boards/nrf52840dk.toml declares.
    assert_eq!(v["app_ram"]["start"], 0x2000_0000u64);
    assert_eq!(v["kernel"], Value::Null);
}

/// The point of the whole exercise. `boards/nrf52840dk.toml` declares app
/// RAM from 0x20000000, but the kernel reports it starting 33.8 KiB higher.
/// An app fixed to a RAM address in that gap cannot run, and until the
/// kernel was asked it was installed anyway.
#[test]
fn an_app_fixed_into_kernel_ram_is_refused_once_the_kernel_is_asked() {
    let d = Dir::new("kernel-ram-gap");
    // 0x20001000 is inside the board file's claim and below the kernel's.
    let gap = d.tab_ram("gapapp", 2048, Some(0x4_0400), 0x2000_1000);
    let gap = gap.display().to_string();

    let with_kernel = d.with_kernel("kernel.bin", &kernel_attributes());
    let without = d.erased("erased.bin");

    let refused = tock(
        &d,
        &on_file(
            &format!("path={}", with_kernel.display()),
            &["install", "--dry-run", &gap],
        ),
    );
    assert!(!refused.status.success(), "must be refused");
    let why = String::from_utf8_lossy(&refused.stderr);
    assert!(why.contains("fixed RAM address"), "{why}");
    // The number, and whose it is -- otherwise there is nothing to act on.
    assert!(why.contains("0x20008704"), "{why}");
    assert!(why.contains("according to the kernel"), "{why}");

    // The same TAB on a board that cannot say otherwise still installs, so
    // this is the kernel's doing and not a new blanket restriction.
    let allowed = tock(
        &d,
        &on_file(
            &format!("path={}", without.display()),
            &["install", "--dry-run", &gap],
        ),
    );
    assert!(
        allowed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
}

/// Asking the kernel costs nothing: the attribute table ends where the app
/// region begins, so one request covers the window and the first header
/// probe together. Without this the feature quietly doubles the round trips
/// of every command.
#[test]
fn asking_the_kernel_costs_no_extra_round_trip() {
    let d = Dir::new("kernel-round-trips");
    let image = d.with_kernel("flash.bin", &kernel_attributes());
    let opt = format!("path={}", image.display());
    let out = tock(
        &d,
        &on_file(&opt, &["--trace-protocol", "--format", "json", "list"]),
    );
    let trace = String::from_utf8_lossy(&out.stderr);
    // One leg of the conversation: `tock` also relays each message.
    let reads = trace
        .lines()
        .filter(|l| l.starts_with("transport -> ") && l.contains(r#""op":"read""#))
        .count();
    assert_eq!(reads, 1, "one read for an empty board:\n{trace}");
}

// --- transport options ---------------------------------------------------

/// The bug this exists to fix. `crate` is a typo for `create`; it used to be
/// resolved, given provenance, serialised into `Open` and then ignored, and
/// the command succeeded as though nothing had been asked for.
#[test]
fn a_typo_in_an_option_is_refused_not_swallowed() {
    let d = Dir::new("option-typo");
    let image = d.erased("flash.bin");
    let opt = format!("path={}", image.display());
    let out = tock(
        &d,
        &on_file(&opt, &["--opt", "crate=true", "--format", "json", "list"]),
    );
    assert!(!out.status.success(), "a typo must not succeed");
    let v: Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert!(v["error"].as_str().unwrap().contains("`crate`"), "{v}");
    // Where it came from is context, and what to do about it is a hint.
    let details = v["details"].as_array().unwrap();
    assert!(
        details
            .iter()
            .any(|d| d.as_str().unwrap().contains("flag --opt crate")),
        "{v}"
    );
    let hints = v["hints"].as_array().unwrap();
    assert!(
        hints
            .iter()
            .any(|h| h.as_str().unwrap().contains("did you mean `create`")),
        "{v}"
    );
}

/// A key nothing resembles gets no guess. A wrong suggestion is worse than
/// none, and the list of what is actually read is the useful part.
#[test]
fn an_unrecognisable_option_is_refused_without_a_guess() {
    let d = Dir::new("option-nonsense");
    let image = d.erased("flash.bin");
    let opt = format!("path={}", image.display());
    let out = tock(&d, &on_file(&opt, &["--opt", "nonsense=42", "list"]));
    assert!(!out.status.success());
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(why.contains("`nonsense`"), "{why}");
    assert!(!why.contains("did you mean"), "no plausible guess: {why}");
    assert!(why.contains("path, create"), "{why}");
}

/// Declared options still work, so the check refuses only what it should.
#[test]
fn declared_options_are_accepted() {
    let d = Dir::new("option-declared");
    let image = d.path("made.bin");
    let out = tock(
        &d,
        &on_file(
            &format!("path={}", image.display()),
            &["--opt", "create=true", "list"],
        ),
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(image.exists(), "`create` was honoured");
}

/// The division of labour: the transport knows a value is wrong and says
/// which key, and `tock` -- the only end that resolved it -- says which file
/// wrote it. Neither sentence is much use alone.
#[test]
fn a_bad_value_is_reported_with_where_it_came_from() {
    let d = Dir::new("option-provenance");
    std::fs::write(
        d.path("Tock.toml"),
        "board = \"nrf52840dk\"\ntransport = \"file\"\n\n[transports.file]\npath = \"missing.bin\"\n",
    )
    .unwrap();
    let out = tock(&d, &["--format", "json", "list"]);
    assert!(!out.status.success());
    let v: Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();

    let details: Vec<&str> = v["details"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap())
        .collect();
    // `path` was set, and we say by which file.
    assert!(
        details
            .iter()
            .any(|d| d.contains("path =") && d.contains("Tock.toml")),
        "{details:?}"
    );
    // `create` was named by the transport but never set, which is also worth
    // saying -- it is the one the user is about to be told to add.
    assert!(
        details
            .iter()
            .any(|d| d.contains("`create`") && d.contains("not set")),
        "{details:?}"
    );
}

/// `tock transports` asks each one what it is. `Hello` is answered before a
/// transport touches a device, so this works with nothing plugged in --
/// which is the point, since it is what you run to find out what to plug in.
#[test]
fn transports_lists_what_each_one_reads() {
    let d = Dir::new("transports-listing");
    let out = ok(tock(&d, &["--format", "json", "transports"]));
    let v: Value = serde_json::from_str(&out).unwrap();
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"file"), "{names:?}");
    assert!(names.contains(&"openocd"), "{names:?}");

    let file = v
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "file")
        .unwrap();
    let options: Vec<&str> = file["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert_eq!(options, vec!["path", "create"]);
    // The same list the refusal above quotes, from the same source.
    assert_eq!(file["capabilities"][0], "memory_read");
}

// --- console -------------------------------------------------------------

/// A pseudo-terminal pair, which is a serial port for every purpose this
/// transport has: `termios` applies, `poll` works, and bytes written to one
/// end arrive at the other. It stands in for a board without needing one.
fn pty() -> (std::fs::File, std::fs::File, PathBuf) {
    use std::os::fd::FromRawFd;
    let mut manager = 0;
    let mut device = 0;
    // SAFETY: two out-parameters we own, and null for the optional settings.
    let made = unsafe {
        libc::openpty(
            &mut manager,
            &mut device,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(made, 0, "openpty");
    // SAFETY: freshly opened by `openpty` and not owned elsewhere.
    let name = unsafe {
        let raw = libc::ptsname(manager);
        assert!(!raw.is_null(), "ptsname");
        std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned()
    };
    // Both sides stay open. A pty with no device side open answers reads
    // on the manager with EIO, and the transport opens the device by name
    // in another process -- so this one is held purely to keep the pair
    // alive, and never read from.
    // SAFETY: both are ours, freshly opened, and closed nowhere else.
    unsafe {
        (
            std::fs::File::from_raw_fd(manager),
            std::fs::File::from_raw_fd(device),
            PathBuf::from(name),
        )
    }
}

/// The whole path: a transport that declares only `console`, a client that
/// requires it, and bytes travelling in both directions over one strictly
/// synchronous connection.
#[test]
fn the_console_carries_bytes_both_ways() {
    use std::io::{Read, Write};

    let d = Dir::new("console-pty");
    let (mut board, _keepalive, port) = pty();

    let mut child = Command::new(env!("CARGO_BIN_EXE_tock"))
        .args([
            "--board",
            "nrf52840dk",
            "--transport",
            "serial",
            "--opt",
            &format!("port={}", port.display()),
            // No pacing: a pty keeps up, and the test should not wait.
            "--opt",
            "write_pace_ms=0",
            "console",
        ])
        .current_dir(&d.0)
        .env("TOCK_CONFIG", d.path("no-user-config.toml"))
        .env("TOCK_PROJECT", d.path("Tock.toml"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // The transport has to spawn, greet and open before any of this lands.
    std::thread::sleep(std::time::Duration::from_millis(400));

    // Device to terminal.
    board.write_all(b"tock$ ").unwrap();
    board.flush().unwrap();

    // Terminal to device: the client forwards whatever it is given.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"list\n").unwrap();
    stdin.flush().unwrap();

    // Non-blocking, so a quiet moment is not a hang.
    // SAFETY: a descriptor we own.
    unsafe { libc::fcntl(board.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    let mut heard = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !heard.windows(5).any(|w| w == b"list\n") {
        let mut buffer = [0u8; 256];
        match board.read(&mut buffer) {
            Ok(0) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Ok(n) => heard.extend_from_slice(&buffer[..n]),
            // Nothing yet, or the device side is between opens.
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    let _ = child.kill();
    let printed = child.wait_with_output().unwrap().stdout;

    assert!(
        heard.windows(5).any(|w| w == b"list\n"),
        "the device never heard it: {:?}",
        String::from_utf8_lossy(&heard)
    );
    assert!(
        printed.windows(6).any(|w| w == b"tock$ "),
        "the terminal never showed it: {:?}",
        String::from_utf8_lossy(&printed)
    );
}

/// A transport that cannot do consoles says so, rather than the client
/// discovering it one request later.
#[test]
fn a_transport_without_a_console_refuses_up_front() {
    let d = Dir::new("console-unsupported");
    let image = d.erased("flash.bin");
    let opt = format!("path={}", image.display());
    let out = tock(&d, &on_file(&opt, &["console"]));
    assert!(!out.status.success());
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(why.contains("console"), "{why}");
    assert!(why.contains("capability"), "{why}");
}
