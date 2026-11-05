//! Tests for TBF and TAB parsing.

use super::*;

#[test]
fn round_trips_byte_for_byte() {
    let tbf = testing::app("blink", 2048, 1000, None);
    let bytes = tbf.to_bytes(0xFF);
    let parsed = Tbf::parse(&bytes).unwrap();
    assert!(parsed.header.checksum_ok());
    assert_eq!(parsed.to_bytes(0xFF), bytes);
    assert_eq!(parsed.header.package_name(), Some("blink"));
    assert!(parsed.header.is_app());
}

#[test]
fn unknown_tlvs_are_preserved() {
    let mut tbf = testing::app("x", 1024, 10, None);
    tbf.header.tlvs.push(Tlv::Raw {
        tlv_type: 0x42,
        data: vec![1, 2, 3],
    });
    tbf.header.header_size = tbf.header.to_bytes().len() as u16;
    let bytes = tbf.to_bytes(0xFF);
    let parsed = Tbf::parse(&bytes).unwrap();
    assert_eq!(
        parsed.header.tlvs.last(),
        Some(&Tlv::Raw {
            tlv_type: 0x42,
            data: vec![1, 2, 3]
        })
    );
    assert_eq!(parsed.to_bytes(0xFF), bytes);
}

#[test]
fn padding_is_not_an_app() {
    let p = Header::padding(4096);
    let parsed = Header::parse(&p.to_bytes()).unwrap();
    assert!(!parsed.is_app());
    assert!(parsed.checksum_ok());
    assert_eq!(parsed.total_size, 4096);
}

#[test]
fn erased_flash_is_not_a_header() {
    assert_eq!(Lengths::parse(&[0xFF; 16]), None);
    assert_eq!(Header::parse(&[0xFF; 16]), Err(Error::NotAHeader));
}

#[test]
fn fixed_placement_moves_binary() {
    let mut tbf = testing::app("fixed", 4096, 100, Some((0x2000_8000, 0x4_0480)));
    let hs = tbf.header.header_size as u32;
    tbf.place_fixed_at(0x4_0000).unwrap();
    assert_eq!(0x4_0000 + tbf.header.size_before_app(), 0x4_0480);
    let bytes = tbf.to_bytes(0xFF);
    let start = tbf.header.size_before_app() as usize;
    assert_eq!(bytes[start], tbf.after_header[start - hs as usize]);
    assert!(Tbf::parse(&bytes).unwrap().header.checksum_ok());
    // Too late to reach the fixed address.
    assert!(
        testing::app("f", 4096, 100, Some((0, 0x4_0010)))
            .place_fixed_at(0x4_0000)
            .is_err()
    );
}

#[test]
fn tab_round_trip_and_arch_aliases() {
    let a = testing::app("blink", 1024, 10, None).to_bytes(0xFF);
    let tab = Tab::build(
        "tab-version = 1\nname = \"blink\"\n",
        &[("cortex-m4.tbf", &a), ("riscv32imc.tbf", &a)],
    );
    let tab = Tab::parse(&tab).unwrap();
    assert_eq!(tab.name(), Some("blink"));
    assert_eq!(tab.architectures(), vec!["cortex-m4", "riscv32imc"]);
    assert_eq!(tab.tbfs_for_arch("cortex-m4").unwrap().len(), 1);
    assert_eq!(tab.tbfs_for_arch("rv32imc").unwrap().len(), 1);
    assert_eq!(tab.tbfs_for_arch("cortex-m0").unwrap().len(), 0);
}
