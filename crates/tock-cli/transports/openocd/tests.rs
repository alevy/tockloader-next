//! Tests for the openocd transport's pure parts: sector arithmetic, config resolution, and reading openocd's replies.

use super::*;

use std::collections::BTreeMap;

fn reply(raw: &str) -> TclReply {
    TclReply::parse(raw).expect("parses")
}

#[test]
fn a_page_size_that_cannot_be_an_erase_unit_is_refused() {
    // Zero would divide by zero in sector_span; a non-power-of-two
    // silently straddles real sectors and erases the neighbours.
    assert!(page_size_from(Some(4096)).is_ok());
    assert!(page_size_from(Some(2048)).is_ok());
    assert!(page_size_from(Some(0)).is_err());
    assert!(page_size_from(Some(1000)).is_err());
    assert!(page_size_from(None).is_err());
}

#[test]
fn sector_span_covers_every_touched_sector() {
    // Mid-sector start and end: one sector.
    assert_eq!(sector_span(4096, 0x1010, 16), Some((0x1000, 4096)));
    // Spanning a boundary: both sectors, so both partial ends survive.
    assert_eq!(sector_span(4096, 0x1ff0, 0x20), Some((0x1000, 8192)));
    // Exactly whole sectors: unchanged.
    assert_eq!(sector_span(4096, 0x2000, 8192), Some((0x2000, 8192)));
    // Empty writes touch nothing.
    assert_eq!(sector_span(4096, 0x1010, 0), None);
}

#[test]
fn only_sector_aligned_whole_sector_writes_skip_the_merge() {
    use WritePlan::*;
    assert!(matches!(write_plan(4096, 0x1010, 16), Some(Merge { .. })));
    assert!(matches!(write_plan(4096, 0x1000, 100), Some(Merge { .. })));
    assert!(matches!(write_plan(4096, 0x2000, 8192), Some(Direct)));
    // Nothing to write, whether or not the address is sector-aligned.
    assert_eq!(write_plan(4096, 0x2000, 0), None);
    assert_eq!(write_plan(4096, 0x2010, 0), None);
}

#[test]
fn parses_a_plain_status() {
    let r = reply("0\u{1f}\u{1f}0");
    assert_eq!(r.rc, 0);
    assert!(!r.failed());
}

#[test]
fn reads_the_code_past_a_tcl_error_prefix() {
    // A TCL-level error prefixes the status field with openocd's log line.
    let r = reply("command.c:708: Error: 1\u{1f}Division by zero\u{1f}1\nat file \"x\"");
    assert_eq!(r.rc, 1);
    assert_eq!(r.message(), "Division by zero");
}

#[test]
fn falls_back_to_the_log_when_there_is_no_message() {
    let r = reply("-101\u{1f}\u{1f}Debug Adapter has to be specified");
    assert_eq!(r.rc, -101);
    assert!(r.failed());
    assert_eq!(r.message(), "Debug Adapter has to be specified");
}

#[test]
fn ignores_a_log_that_is_only_the_status_echo() {
    // `capture` returns the script's result when the command logged
    // nothing, and that result is the rc -- which says nothing.
    let r = reply("-4\u{1f}\u{1f}-4");
    assert_eq!(r.message(), "openocd returned -4");
}

#[test]
fn rejects_a_reply_it_cannot_read() {
    assert!(TclReply::parse("not a reply").is_err());
    assert!(TclReply::parse("x\u{1f}y\u{1f}z").is_err());
}

#[test]
fn config_entries_resolve_next_to_the_file_that_named_them() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("local.cfg"), "").unwrap();

    // Names a real file beside the board file: use it.
    assert_eq!(
        resolve_config_entry("local.cfg", Some(dir.path())),
        dir.path().join("local.cfg").display().to_string()
    );
    // Not a file there: leave it for openocd's script search path.
    assert_eq!(
        resolve_config_entry("board/nordic_nrf52_dk.cfg", Some(dir.path())),
        "board/nordic_nrf52_dk.cfg"
    );
    // No provenance at all, and absolute paths, pass through.
    assert_eq!(resolve_config_entry("local.cfg", None), "local.cfg");
    assert_eq!(
        resolve_config_entry("/a/b.cfg", Some(dir.path())),
        "/a/b.cfg"
    );
}

#[test]
fn probe_and_target_are_two_halves_with_two_owners() {
    let bases = BTreeMap::new();
    let configs = |v: serde_json::Value| {
        let map = v.as_object().unwrap().clone();
        resolve_configs(&Options::new(&map, &bases))
    };
    let file = |p: &str| Step::File(p.into());
    let command = |c: &str| Step::Command(c.into());

    // The whole point: the board file knows the chip, the user config knows
    // what is plugged in, and they arrive as separate keys so neither has to
    // restate the other. `probe` runs first -- the only order openocd takes.
    assert_eq!(
        configs(serde_json::json!({
            "probe": ["interface/jlink.cfg", {"command": "transport select swd"}],
            "target": ["target/nrf52.cfg"],
        }))
        .unwrap(),
        vec![
            file("interface/jlink.cfg"),
            command("transport select swd"),
            file("target/nrf52.cfg"),
        ]
    );

    // Either half alone is fine: a board with no probe default, or a probe
    // with no board yet.
    assert_eq!(
        configs(serde_json::json!({"target": "target/nrf52.cfg"})).unwrap(),
        vec![file("target/nrf52.cfg")]
    );
    assert_eq!(
        configs(serde_json::json!({"probe": "interface/jlink.cfg"})).unwrap(),
        vec![file("interface/jlink.cfg")]
    );
    assert!(configs(serde_json::json!({})).unwrap().is_empty());

    // `config` is the escape hatch for a board that does not decompose.
    assert_eq!(
        configs(serde_json::json!({"config": ["board/nordic_nrf52_dk.cfg"]})).unwrap(),
        vec![file("board/nordic_nrf52_dk.cfg")]
    );
    // But it is the whole stage, so mixing it with a half is a mistake we
    // must not resolve by guessing which one the user meant.
    let both = configs(serde_json::json!({
        "config": ["board/nordic_nrf52_dk.cfg"],
        "probe": ["interface/jlink.cfg"],
    }));
    assert!(both.is_err(), "config + probe was silently accepted");

    // And that is what openocd is told, in that order.
    let steps = configs(serde_json::json!({
        "probe": ["a.cfg", {"command": "transport select swd"}],
        "target": ["b.cfg"],
    }))
    .unwrap();
    let args: Vec<&str> = steps.iter().flat_map(Step::args).collect();
    assert_eq!(
        args,
        ["-f", "a.cfg", "-c", "transport select swd", "-f", "b.cfg"]
    );

    // An entry we cannot read is an error, not a silent omission.
    assert!(configs(serde_json::json!({"target": [{"fle": "typo.cfg"}]})).is_err());
    assert!(configs(serde_json::json!({"probe": [42]})).is_err());
}

/// openocd programs whatever address it is handed, so this check is the
/// only thing between a wrong address and the kernel. It must come
/// before the session is touched -- otherwise a bad range on a closed
/// transport reports "not open" and the real problem goes unsaid.
#[test]
fn an_access_outside_flash_is_refused_before_openocd_is_asked() {
    let mut t = OpenocdTransport {
        session: None,
        flash: Some(FlashInfo {
            start: 0x8000_0000,
            size: 0x4_0000,
        }),
    };
    let refusal = |r: Result<_, Failure>| match r {
        Err(Failure::Operation(response)) => match *response {
            Response::Error { message, .. } => message,
            other => panic!("{other:?}"),
        },
        _ => panic!("expected a refusal"),
    };
    // Below the region, past the end, and straddling the end.
    let message = refusal(t.read(0x7fff_ffff, 4));
    assert!(message.contains("outside flash"), "{message}");
    assert!(
        message.contains("0x8000_0000".replace('_', "").as_str()),
        "{message}"
    );
    let message = refusal(t.write(0x8010_0000, &[0u8; 4]).map(|_| Vec::new()));
    assert!(message.contains("outside flash"), "{message}");
    let message = refusal(t.read(0x8003_fffe, 4));
    assert!(message.contains("outside flash"), "{message}");
    // And an access inside it gets past the check, to fail on the session
    // instead -- which is how we know the check is not refusing everything.
    let message = refusal(t.read(0x8002_0000, 4));
    assert!(message.contains("not open"), "{message}");
}

/// The session halts the target to do its work, so how it is left is the
/// difference between a board that runs afterwards and one that does not.
/// The command itself needs hardware; which command is chosen does not.
#[test]
fn a_session_that_wrote_resets_and_one_that_only_read_resumes() {
    assert_eq!(parting_command(true), "reset run");
    assert_eq!(parting_command(false), "resume");
}
