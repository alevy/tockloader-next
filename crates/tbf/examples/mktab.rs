//! Write a synthetic TAB for testing: `mktab <name> <total_size> <binary_len> <arch> <out.tab> [fixed_flash]`
fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 6 {
        eprintln!(
            "usage: mktab <name> <total_size> <binary_len> <arch> <out.tab> [fixed_flash_hex]"
        );
        std::process::exit(2);
    }
    let fixed = a.get(6).map(|f| {
        (
            0x2000_8000,
            u32::from_str_radix(f.trim_start_matches("0x"), 16).unwrap(),
        )
    });
    let tbf = tbf::testing::app(&a[1], a[2].parse().unwrap(), a[3].parse().unwrap(), fixed);
    let bytes = tbf.content_bytes();
    let meta = format!("tab-version = 1\nname = \"{}\"\n", a[1]);
    let name = format!("{}.tbf", a[4]);
    std::fs::write(&a[5], tbf::Tab::build(&meta, &[(&name, &bytes)])).unwrap();
}
