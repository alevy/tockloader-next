//! Tests for the kernel attribute table.
//!
//! The first two run against bytes taken verbatim from kernels built out of
//! the Tock tree, which is the only way to be sure of a field order or an
//! endianness. The rest are synthetic, and each one pins a rule about where
//! the walk stops -- the part of this format with no specification behind it.

use super::*;

/// The last 256 bytes of a real `nrf52840dk` kernel region.
const NRF52840DK: &[u8] = include_bytes!("../../tests/fixtures/nrf52840dk.attrs.bin");

/// The same, for a board whose flash is not at zero.
const LORA_E5_MINI: &[u8] = include_bytes!("../../tests/fixtures/lora_e5_mini.attrs.bin");

#[test]
fn a_real_nrf52840dk_kernel() {
    let a = parse(NRF52840DK).expect("sentinel is there");
    assert_eq!(a.version, VERSION);
    assert!(a.understood());
    assert_eq!(
        a.app_ram,
        Some(Region {
            start: 0x2000_8704,
            size: 227_580
        })
    );
    // The *binary*, which is far smaller than the 256 KiB region reserved
    // for it -- which is why the app address cannot be derived from this.
    assert_eq!(
        a.kernel_binary,
        Some(Region {
            start: 0x0,
            size: 172_032
        })
    );
    assert_eq!(a.kernel_version.unwrap().to_string(), "2.4.0-dev");
    assert!(a.other.is_empty(), "nothing unmodelled: {:?}", a.other);
}

#[test]
fn a_real_lora_e5_mini_kernel() {
    let a = parse(LORA_E5_MINI).expect("sentinel is there");
    assert_eq!(
        a.app_ram,
        Some(Region {
            start: 0x2000_4454,
            size: 48_044
        })
    );
    assert_eq!(
        a.kernel_binary,
        Some(Region {
            start: 0x0800_0000,
            size: 75_776
        })
    );
    assert_eq!(a.kernel_version.unwrap().to_string(), "2.4.0-dev");
}

/// What every board file in this repo claims, against what the kernel on the
/// board actually reports. The board file is not merely imprecise: a TBF
/// fixed at a RAM address in the gap would be accepted and could not run.
#[test]
fn the_board_file_over_claims_app_ram() {
    let kernel = parse(NRF52840DK).unwrap().app_ram.unwrap();
    let board_file = Region {
        start: 0x2000_0000,
        size: 256 * 1024,
    };
    assert!(kernel.start > board_file.start);
    assert_eq!(kernel.end(), board_file.end(), "they agree about the top");
    assert_eq!(kernel.start - board_file.start, 34_564);
}

fn canonical() -> Attributes {
    Attributes {
        version: VERSION,
        app_ram: Some(Region {
            start: 0x2000_8704,
            size: 227_580,
        }),
        kernel_binary: Some(Region {
            start: 0,
            size: 172_032,
        }),
        kernel_version: Some(Version {
            major: 2,
            minor: 4,
            patch: 0,
            prerelease: 1,
        }),
        other: Vec::new(),
    }
}

#[test]
fn to_bytes_matches_what_a_kernel_emits() {
    // Not merely a round trip: the bytes must equal the real table, so the
    // builder the tests elsewhere rely on cannot drift from the format.
    let real = &NRF52840DK[NRF52840DK.len() - 44..];
    assert_eq!(canonical().to_bytes(), real);
}

#[test]
fn a_canonical_table_survives_a_round_trip() {
    assert_eq!(parse(&canonical().to_bytes()), Some(canonical()));
}

/// A table is found by its last four bytes and nothing else.
#[test]
fn no_sentinel_is_not_an_error() {
    assert_eq!(parse(&[0xFF; 64]), None);
    assert_eq!(parse(&[0x00; 64]), None);
    let mut bytes = canonical().to_bytes();
    let n = bytes.len();
    bytes[n - 1] = b'X';
    assert_eq!(parse(&bytes), None);
}

#[test]
fn a_window_too_small_to_hold_a_header_is_not_a_table() {
    assert_eq!(parse(b"TOCK"), None);
    assert_eq!(parse(b""), None);
}

/// Padding below the table is `0x00` when the kernel was flashed from its
/// `.bin` and `0xFF` when it was flashed from its `.elf`, because the
/// attributes are their own load segment with an erased gap beneath. Both
/// have to end the walk, and neither is special-cased: zero length cannot
/// advance, and 65535 bytes cannot fit.
#[test]
fn both_kinds_of_padding_end_the_walk() {
    for pad in [0x00u8, 0xFF] {
        let mut bytes = vec![pad; 64];
        bytes.extend_from_slice(&canonical().to_bytes());
        let a = parse(&bytes).unwrap_or_else(|| panic!("pad {pad:#x}"));
        assert_eq!(a, canonical(), "pad {pad:#x}");
    }
}

/// The reason unknown types are skipped rather than treated as the end.
/// The kernel emits app memory nearest the sentinel, so a type added
/// upstream would be met first; stopping there would hide the one value we
/// most need. Python tockloader stops.
#[test]
fn an_unknown_type_nearest_the_sentinel_does_not_hide_the_rest() {
    let mut table = canonical().to_bytes();
    // Splice a 64-byte unknown TLV in just below the header, i.e. above
    // every TLV the kernel wrote.
    let at = table.len() - 8;
    let mut unknown = vec![0xAB; 64];
    unknown.extend_from_slice(&0x0105u16.to_le_bytes());
    unknown.extend_from_slice(&64u16.to_le_bytes());
    table.splice(at..at, unknown);

    let mut padded = vec![0xFF; 32];
    padded.extend_from_slice(&table);
    let a = parse(&padded).expect("sentinel");
    assert_eq!(a.app_ram, canonical().app_ram, "reached past the unknown");
    assert_eq!(a.kernel_version, canonical().kernel_version);
    assert_eq!(a.other.len(), 1);
    assert_eq!(a.other[0].tlv_type, 0x0105);
    assert_eq!(a.other[0].value, vec![0xAB; 64]);
}

/// A known type carrying the wrong number of bytes is kept, not guessed at:
/// inventing an address out of four bytes is worse than admitting ignorance.
#[test]
fn a_known_type_with_a_wrong_length_is_not_decoded() {
    let mut table = Vec::new();
    table.extend_from_slice(&[0u8; 4]); // four bytes, not eight
    table.extend_from_slice(&tlv::APP_MEMORY.to_le_bytes());
    table.extend_from_slice(&4u16.to_le_bytes());
    table.extend_from_slice(&[0, 0, 0, VERSION]);
    table.extend_from_slice(SENTINEL);

    let a = parse(&table).expect("sentinel");
    assert_eq!(a.app_ram, None, "not decoded from the wrong length");
    assert_eq!(a.other.len(), 1);
    assert_eq!(a.other[0].tlv_type, tlv::APP_MEMORY);
}

/// A length that runs off the bottom stops the walk instead of panicking.
#[test]
fn a_length_past_the_floor_stops_rather_than_panicking() {
    let mut table = Vec::new();
    table.extend_from_slice(&[0x11; 8]);
    table.extend_from_slice(&0x0999u16.to_le_bytes());
    table.extend_from_slice(&60_000u16.to_le_bytes()); // far more than is here
    table.extend_from_slice(&[0, 0, 0, VERSION]);
    table.extend_from_slice(SENTINEL);

    let a = parse(&table).expect("sentinel");
    assert!(a.other.is_empty());
    assert_eq!(a.app_ram, None);
}

/// Nothing may be read more than `MAX_TABLE` below the end, however
/// plausible the bytes look.
#[test]
fn the_walk_is_bounded() {
    // A chain of 8-byte unknown TLVs long enough to run past the bound.
    let mut chain = Vec::new();
    for _ in 0..(MAX_TABLE / 12 + 20) {
        chain.extend_from_slice(&[0x22; 8]);
        chain.extend_from_slice(&0x0800u16.to_le_bytes());
        chain.extend_from_slice(&8u16.to_le_bytes());
    }
    chain.extend_from_slice(&[0, 0, 0, VERSION]);
    chain.extend_from_slice(SENTINEL);

    let a = parse(&chain).expect("sentinel");
    let read: usize = a.other.iter().map(|t| t.value.len() + 4).sum();
    assert!(
        read as u64 <= MAX_TABLE,
        "walked {read} bytes, bound is {MAX_TABLE}"
    );
}

/// A version we do not know is still parsed. The framing has already
/// outlived one bump, and refusing would make a newer kernel unreadable to
/// an older tool for nothing.
#[test]
fn an_unknown_version_is_reported_not_refused() {
    let mut odd = canonical();
    odd.version = 7;
    let a = parse(&odd.to_bytes()).expect("sentinel");
    assert_eq!(a.version, 7);
    assert!(!a.understood());
    assert_eq!(a.app_ram, canonical().app_ram, "still read");
}

#[test]
fn version_renders_the_way_tock_writes_it() {
    let v = |prerelease| {
        Version {
            major: 2,
            minor: 4,
            patch: 1,
            prerelease,
        }
        .to_string()
    };
    assert_eq!(v(0), "2.4.1");
    assert_eq!(v(1), "2.4.1-dev");
    assert_eq!(v(2), "2.4.1-alpha");
    assert_eq!(v(3), "2.4.1-beta");
    assert_eq!(v(9), "2.4.1-pre9");
}

// --- the Flash/NeedRead layer -------------------------------------------

/// The window is bounded below by the floor the caller permits, because
/// reading outside the flash a transport was opened for is a hard failure,
/// not a miss.
#[test]
fn the_window_is_clamped_to_the_floor() {
    let w = window(0x40000, 0x0).expect("room");
    assert_eq!(w.start, 0x40000 - MAX_TABLE);
    assert_eq!(w.end(), 0x40000);

    let tight = window(0x100, 0x80).expect("room");
    assert_eq!(tight.start, 0x80, "floor wins over MAX_TABLE");
    assert_eq!(tight.size, 0x80);
}

#[test]
fn no_room_for_a_table_means_no_window() {
    assert_eq!(window(0x40000, 0x40000), None);
    assert_eq!(window(0x10, 0x8), None, "too small to hold a header");
}

#[test]
fn attributes_asks_for_the_window_then_reads_it() {
    let mut flash = Flash::new();

    // Nothing known yet: it must ask, and for the whole window at once.
    let need = attributes(0x40000, 0x0, &flash).expect_err("must ask");
    assert_eq!(need.ranges, vec![(0x40000 - MAX_TABLE, MAX_TABLE)]);

    // Answer with erased flash holding a real table at the top.
    let mut window_bytes = vec![0xFF; MAX_TABLE as usize];
    let table = canonical().to_bytes();
    let at = window_bytes.len() - table.len();
    window_bytes[at..].copy_from_slice(&table);
    flash.insert(0x40000 - MAX_TABLE, &window_bytes);

    let a = attributes(0x40000, 0x0, &flash).expect("no more reads");
    assert_eq!(a, Some(canonical()));
}

#[test]
fn a_board_with_no_kernel_reads_as_no_attributes() {
    let mut flash = Flash::new();
    flash.insert(0x40000 - MAX_TABLE, &vec![0xFF; MAX_TABLE as usize]);
    assert_eq!(attributes(0x40000, 0x0, &flash), Ok(None));
}
