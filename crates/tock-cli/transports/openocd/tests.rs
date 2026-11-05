//! Tests for the openocd transport's pure parts: sector arithmetic, config resolution, and reading openocd's replies.

use super::*;

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
fn config_accepts_a_bare_string_as_well_as_a_list() {
    let bases = std::collections::BTreeMap::new();
    let one = serde_json::json!({"config": "a.cfg"});
    let many = serde_json::json!({"config": ["a.cfg", "b.cfg"]});
    let none = serde_json::json!({});
    assert_eq!(
        resolve_configs(&Options::new(one.as_object().unwrap(), &bases)),
        vec!["a.cfg"]
    );
    assert_eq!(
        resolve_configs(&Options::new(many.as_object().unwrap(), &bases)),
        vec!["a.cfg", "b.cfg"]
    );
    assert!(resolve_configs(&Options::new(none.as_object().unwrap(), &bases)).is_empty());
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
