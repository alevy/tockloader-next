//! Tests for size strings like `256K`.

use super::Size;

#[test]
fn parses() {
    assert_eq!(Size::parse("4K"), Some(4096));
    assert_eq!(Size::parse("1M"), Some(1 << 20));
    assert_eq!(Size::parse("0x4_0000"), Some(0x40000));
    assert_eq!(Size::parse("12"), Some(12));
    assert_eq!(Size::parse("4KB"), None);
}
