//! Tests for the wire format, the two roles, and the option helpers.

use super::*;

struct Echo;
impl Connection for Echo {
    fn call(
        &mut self,
        request: Request,
        payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        Ok(match request {
            Request::Read { len, .. } => (Response::Data, vec![0xAB; len as usize]),
            Request::Write { .. } => (Response::Ok, vec![]),
            Request::Close => (Response::Ok, vec![]),
            _ => (Response::error(ErrorKind::Unsupported, "nope"), payload),
        })
    }
}

#[test]
fn frames_round_trip_over_a_stream() {
    // Client writes requests into a buffer; server reads them and writes
    // responses into another buffer; client reads those.
    let mut to_server = Vec::new();
    write_frame(&mut to_server, &Request::Read { address: 0, len: 4 }, &[]).unwrap();
    write_frame(&mut to_server, &Request::Write { address: 0 }, &[1, 2, 3]).unwrap();
    write_frame(&mut to_server, &Request::Reset, &[]).unwrap();
    write_frame(&mut to_server, &Request::Close, &[]).unwrap();
    let mut to_client = Vec::new();
    serve(&to_server[..], &mut to_client, &mut Echo).unwrap();

    let mut r = &to_client[..];
    let (resp, data): (Response, _) = read_frame(&mut r).unwrap().unwrap();
    assert_eq!((resp, data), (Response::Data, vec![0xAB; 4]));
    let (resp, _): (Response, Vec<u8>) = read_frame(&mut r).unwrap().unwrap();
    assert_eq!(resp, Response::Ok);
    let (resp, _): (Response, Vec<u8>) = read_frame(&mut r).unwrap().unwrap();
    assert!(matches!(
        resp,
        Response::Error {
            kind: ErrorKind::Unsupported,
            ..
        }
    ));
}

#[test]
fn json_shape_is_readable() {
    let s = serde_json::to_string(&Request::Read {
        address: 16,
        len: 4,
    })
    .unwrap();
    assert_eq!(s, r#"{"op":"read","address":16,"len":4}"#);
    let s = serde_json::to_string(&Capability::Vendor("acme.provision".into())).unwrap();
    assert_eq!(s, r#"{"vendor":"acme.provision"}"#);
}

#[test]
fn base_dir_exposes_the_directory_a_value_was_written_in() {
    let values = serde_json::json!({"config": ["a.cfg"], "chip": "X"});
    let mut bases = BTreeMap::new();
    bases.insert("config".to_string(), at("/repo"));
    let o = Options::new(values.as_object().unwrap(), &bases);
    // A list-valued option can resolve its own elements.
    assert_eq!(o.base_dir("config"), Some(PathBuf::from("/repo")));
    // No provenance recorded for this key.
    assert_eq!(o.base_dir("chip"), None);
}

#[test]
fn io_errors_convert_into_device_responses() {
    let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
    let r: Response = e.into();
    match r {
        Response::Error { kind, message, .. } => {
            assert_eq!(kind, ErrorKind::Device);
            assert!(message.contains("nope"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn multi_word_wire_names_are_snake_case() {
    let s = serde_json::to_string(&Request::AppsList).unwrap();
    assert_eq!(s, r#"{"op":"apps_list"}"#);
    let s = serde_json::to_string(&Request::AppsRemove { name: "a".into() }).unwrap();
    assert_eq!(s, r#"{"op":"apps_remove","name":"a"}"#);
    let s = serde_json::to_string(&Capability::MemoryRead).unwrap();
    assert_eq!(s, r#""memory_read""#);
    let s = serde_json::to_string(&ErrorKind::InvalidRequest).unwrap();
    assert_eq!(s, r#""invalid_request""#);

    let open = Request::Open {
        board: "b".into(),
        arch: "cortex-m4".into(),
        flash: FlashInfo {
            start: 0,
            size: 1024,
        },
        options: serde_json::Map::new(),
        option_bases: BTreeMap::new(),
    };
    let s = serde_json::to_string(&open).unwrap();
    assert!(s.contains(r#""option_bases":{}"#), "{s}");
}

fn at(base: &str) -> String {
    base.to_string()
}

#[test]
fn get_path_joins_relative_values_onto_the_base() {
    let values = serde_json::json!({"path": "flash.bin", "abs": "/tmp/x.bin", "bare": "y.bin"});
    let mut bases = BTreeMap::new();
    bases.insert("path".to_string(), at("/repo"));
    bases.insert("abs".to_string(), at("/repo"));
    let o = Options::new(values.as_object().unwrap(), &bases);
    assert_eq!(o.get_path("path"), Some(PathBuf::from("/repo/flash.bin")));
    // Absolute values pass through untouched.
    assert_eq!(o.get_path("abs"), Some(PathBuf::from("/tmp/x.bin")));
    // No provenance: the value is used as written.
    assert_eq!(o.get_path("bare"), Some(PathBuf::from("y.bin")));
    assert_eq!(o.get_path("missing"), None);
}

#[test]
fn open_without_option_bases_still_parses() {
    let json = r#"{"op":"open","board":"b","arch":"cortex-m4",
        "flash":{"start":0,"size":1024},"options":{}}"#;
    let r: Request = serde_json::from_str(json).unwrap();
    match r {
        Request::Open { option_bases, .. } => assert!(option_bases.is_empty()),
        other => panic!("{other:?}"),
    }
}

/// Answers every read with fewer bytes than asked for.
struct Stingy;
impl Connection for Stingy {
    fn call(
        &mut self,
        _request: Request,
        _payload: Vec<u8>,
    ) -> Result<(Response, Vec<u8>), ProtoError> {
        Ok((Response::Data, vec![0xFF; 4]))
    }
}

/// A caller loops until it has what it asked for, so a short reply that
/// came back `Ok` would have it ask for the same bytes forever. Saying so
/// is the whole reason `read_ranges` exists rather than a bare `call`.
#[test]
fn a_short_read_is_an_error_not_a_silent_retry() {
    let mut client = Client {
        connection: Box::new(Stingy),
        transport: "stingy".into(),
        capabilities: vec![Capability::MemoryRead],
    };
    let err = client.read_ranges(&[(0x1000, 64)]).unwrap_err();
    assert!(format!("{err}").contains('4'), "{err}");
}
