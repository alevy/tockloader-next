//! End-to-end tests of the `tock` binary against flash image files.
//!
//! The `python_*` tests compare against Python tockloader byte for byte. They
//! run only when `TOCKLOADER_PY` points at a `tockloader` executable.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const FLASH_SIZE: usize = 0x10_0000; // nRF52840: 1 MiB at 0

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
    /// A TAB like elf2tab's, for `cortex-m4`, optionally built for a fixed
    /// flash address.
    fn tab(&self, name: &str, total: u32, fixed_flash: Option<u32>) -> PathBuf {
        let tbf = tbf::testing::app(
            name,
            total,
            total / 2,
            fixed_flash.map(|f| (0x2000_8000, f)),
        );
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
        shown.lines().any(|l| l.trim_start().starts_with("path =")
            && l.contains("project")),
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

    let out = tock(
        &d,
        &["--board", "nrf52840dk", "--transport", "nosuch", "list"],
    );
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("tock-transport-nosuch"), "{err}");
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
fn plugin_transport_matches_built_in() {
    let d = Dir::new("plugin");
    d.erased("builtin.bin");
    d.erased("plugin.bin");
    let (a, b) = (d.tab("a", 5000, None), d.tab("b", 30000, None));
    let tabs = [a.to_str().unwrap(), b.to_str().unwrap()];
    let mut args = on_file("path=builtin.bin", &["install"]);
    args.extend(tabs);
    ok(tock(&d, &args));
    let mut args = vec![
        "--board",
        "nrf52840dk",
        "--transport",
        "file-plugin",
        "--opt",
        "path=plugin.bin",
        "install",
    ];
    args.extend(tabs);
    ok(tock(&d, &args));
    assert!(
        std::fs::read(d.path("builtin.bin")).unwrap()
            == std::fs::read(d.path("plugin.bin")).unwrap()
    );
}

#[test]
fn broker_serves_the_device() {
    use tock_proto::{Channel, Request, Response, StreamChannel};
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
        .call_ok(
            &Request::Hello {
                protocol: 0,
                client: "test".into(),
            },
            &[],
        )
        .unwrap();
    assert!(matches!(hello, Response::Hello { ref transport, .. } if transport == "file"));
    let (_, data) = ch
        .call_ok(
            &Request::Read {
                address: 0x40000,
                len: 16,
            },
            &[],
        )
        .unwrap();
    let header = tbf::Lengths::parse(&data).unwrap();
    assert_eq!(header.total_size, 8192);
    ch.call_ok(&Request::Close, &[]).unwrap();
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
