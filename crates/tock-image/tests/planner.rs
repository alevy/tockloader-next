//! Planner scenarios run against in-memory flash images.

use tbf::testing::app;
use tock_image::*;

const KB: u64 = 1024;

fn region() -> Region {
    Region {
        start: 0x40000,
        size: 256 * KB,
    }
}

fn cortex_m() -> Placement {
    Placement {
        order: Order::SizeDescending,
        size: SizeRule::PowerOfTwo,
        alignment: Alignment::Size,
    }
}

fn new_app(name: &str, size: u32) -> NewApp {
    NewApp {
        name: name.into(),
        candidates: vec![app(name, size, size / 2, None)],
    }
}

/// Merging distance these scenarios render with. Chosen to match what the CLI
/// currently ships for a 4 KiB erase block, so the tests exercise the same
/// merging the tool does rather than a convenient zero -- but it is this
/// file's own choice of scenario, not a value to keep in step with the CLI's.
const WRITE_GAP: u64 = 4095;

/// A board: flash image covering the app region, initially erased.
struct Board {
    region: Region,
    placement: Placement,
    image: Vec<u8>,
}

impl Board {
    fn new(placement: Placement) -> Board {
        let region = region();
        Board {
            region,
            placement,
            image: vec![0xFF; region.size as usize],
        }
    }

    fn flash(&self) -> Flash {
        Flash::from_image(&self.region, self.region.start, &self.image)
    }

    fn apply(&mut self, req: Request) -> Result<(Plan, Vec<WriteRun>), PlanError> {
        let flash = self.flash();
        let found = discover(&self.region, &flash).unwrap();
        let plan = plan(&self.region, &self.placement, None, &found, &req)?;
        let writes = render(&self.region, &plan, &flash, WRITE_GAP).unwrap();
        for w in &writes {
            let off = (w.address - self.region.start) as usize;
            self.image[off..off + w.data.len()].copy_from_slice(&w.data);
        }
        Ok((plan, writes))
    }

    /// (name, address, size) of each app, as the kernel would find them.
    fn apps(&self) -> Vec<(String, u64, u64)> {
        let found = discover(&self.region, &self.flash()).unwrap();
        found
            .apps()
            .map(|(a, h)| {
                (
                    h.package_name().unwrap().to_string(),
                    a,
                    h.total_size as u64,
                )
            })
            .collect()
    }

    fn assert_valid(&self) {
        let found = discover(&self.region, &self.flash()).unwrap();
        for (address, header) in found.apps() {
            assert!(header.checksum_ok());
            let size = header.total_size as u64;
            if self.placement.alignment == Alignment::Size && header.fixed_addresses().1.is_none() {
                assert_eq!(address % size, 0, "{:?} misaligned", header.package_name());
            }
            if self.placement.size == SizeRule::PowerOfTwo {
                assert!(size.is_power_of_two());
            }
        }
    }
}

fn install(names: &[(&str, u32)]) -> Request {
    Request {
        install: names.iter().map(|(n, s)| new_app(n, *s)).collect(),
        ..Default::default()
    }
}

#[test]
fn install_on_empty_board() {
    let mut b = Board::new(cortex_m());
    let (plan, _) = b.apply(install(&[("blink", 5000)])).unwrap();
    assert_eq!(b.apps(), vec![("blink".into(), 0x40000, 8 * KB)]);
    assert_eq!(
        plan.actions[0],
        Action::Install {
            name: "blink".into(),
            address: 0x40000,
            size: 8 * KB
        }
    );
    b.assert_valid();
}

#[test]
fn largest_first_and_aligned() {
    let mut b = Board::new(cortex_m());
    b.apply(install(&[("small", 2000), ("big", 30000)]))
        .unwrap();
    assert_eq!(
        b.apps(),
        vec![
            ("big".into(), 0x40000, 32 * KB),
            ("small".into(), 0x48000, 2 * KB)
        ]
    );
    b.assert_valid();
}

#[test]
fn installed_apps_do_not_move() {
    let mut b = Board::new(cortex_m());
    b.apply(install(&[("small", 2000)])).unwrap();
    let (plan, _) = b.apply(install(&[("big", 30000)])).unwrap();
    // Python would move `small` to put `big` first. We leave it and align `big`.
    assert_eq!(
        b.apps(),
        vec![
            ("small".into(), 0x40000, 2 * KB),
            ("big".into(), 0x48000, 32 * KB)
        ]
    );
    assert!(plan.actions.contains(&Action::Pad {
        address: 0x40800,
        size: 0x7800
    }));
    b.assert_valid();
}

#[test]
fn uninstall_leaves_others_in_place_and_gap_is_reused() {
    let mut b = Board::new(cortex_m());
    b.apply(install(&[("a", 8000), ("b", 8000), ("c", 8000)]))
        .unwrap();
    let before = b.apps();
    let (_, writes) = b
        .apply(Request {
            uninstall: vec!["b".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(writes.len(), 1, "only the page holding b's header changes");
    let after = b.apps();
    assert_eq!(after, vec![before[0].clone(), before[2].clone()]);

    b.apply(install(&[("d", 7000)])).unwrap();
    assert_eq!(
        b.apps()[1],
        ("d".into(), before[1].1, 8 * KB),
        "d fills b's old slot"
    );
    b.assert_valid();
}

#[test]
fn removing_the_last_app_ends_the_list() {
    let mut b = Board::new(cortex_m());
    b.apply(install(&[("a", 8000), ("b", 4000)])).unwrap();
    let (plan, _) = b
        .apply(Request {
            uninstall: vec!["b".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(plan.end, 0x40000 + 8 * KB);
    assert_eq!(b.apps().len(), 1);
}

#[test]
fn replace_in_place_when_it_fits() {
    let mut b = Board::new(cortex_m());
    b.apply(install(&[("a", 8000), ("b", 8000)])).unwrap();
    let (plan, _) = b.apply(install(&[("a", 7000)])).unwrap();
    assert_eq!(
        plan.actions[0],
        Action::Replace {
            name: "a".into(),
            old_address: 0x40000,
            address: 0x40000,
            size: 8 * KB
        }
    );
    b.assert_valid();
}

#[test]
fn repack_is_opt_in() {
    let mut b = Board::new(Placement::default());
    let k = |n: u64| (n * KB) as u32;
    b.apply(install(&[("a", k(100)), ("b", k(100)), ("c", k(50))]))
        .unwrap();
    b.apply(Request {
        uninstall: vec!["a".into(), "c".into()],
        ..Default::default()
    })
    .unwrap();
    // 156 KiB free, but split into 100 KiB before `b` and 56 KiB after it.
    let err = b.apply(install(&[("d", k(120))])).unwrap_err();
    assert_eq!(
        err,
        PlanError::NeedsRepack {
            name: "d".into(),
            size: 120 * KB
        }
    );

    let req = Request {
        allow_repack: true,
        ..install(&[("d", k(120))])
    };
    let (plan, _) = b.apply(req).unwrap();
    assert!(plan.actions.contains(&Action::Move {
        name: "b".into(),
        from: 0x40000 + 100 * KB,
        to: 0x40000,
        size: 100 * KB
    }));
    assert_eq!(
        b.apps(),
        vec![
            ("b".into(), 0x40000, 100 * KB),
            ("d".into(), 0x40000 + 100 * KB, 120 * KB)
        ]
    );
    // `b` really was copied.
    assert_eq!(
        &b.image[16..64],
        &app("b", k(100), k(50), None).to_bytes(0xFF)[16..64]
    );

    let req = Request {
        allow_repack: true,
        ..install(&[("e", k(200))])
    };
    assert!(matches!(b.apply(req), Err(PlanError::DoesNotFit { .. })));
}

#[test]
fn repack_moves_apps_like_python() {
    let mut b = Board::new(cortex_m());
    b.apply(install(&[("small", 2000)])).unwrap();
    let req = Request {
        allow_repack: true,
        ..install(&[("big", 30000)])
    };
    let (plan, _) = b.apply(req).unwrap();
    assert_eq!(
        b.apps(),
        vec![
            ("big".into(), 0x40000, 32 * KB),
            ("small".into(), 0x48000, 2 * KB)
        ]
    );
    assert!(plan.actions.contains(&Action::Move {
        name: "small".into(),
        from: 0x40000,
        to: 0x48000,
        size: 2 * KB
    }));
    b.assert_valid();
}

#[test]
fn sticky_apps_need_explicit_opt_in() {
    let mut b = Board::new(cortex_m());
    let mut a = new_app("keep", 4000);
    a.candidates[0].header.flags |= tbf::FLAG_STICKY;
    b.apply(Request {
        install: vec![a],
        ..Default::default()
    })
    .unwrap();
    let err = b
        .apply(Request {
            uninstall: vec!["keep".into()],
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(
        err,
        PlanError::Sticky {
            name: "keep".into()
        }
    );
    b.apply(Request {
        uninstall: vec!["keep".into()],
        include_sticky: true,
        ..Default::default()
    })
    .unwrap();
    assert!(b.apps().is_empty());
}

#[test]
fn unknown_app_lists_installed() {
    let mut b = Board::new(cortex_m());
    b.apply(install(&[("a", 1000)])).unwrap();
    let err = b
        .apply(Request {
            uninstall: vec!["nope".into()],
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(
        err,
        PlanError::NotInstalled {
            name: "nope".into(),
            installed: vec!["a".into()]
        }
    );
}

#[test]
fn fixed_address_apps_pick_a_matching_build() {
    let mut b = Board::new(Placement::default());
    // Built for two locations; only the second is inside the region.
    let candidates = vec![
        app("fx", 4096, 1000, Some((0x2000_8000, 0x30080))),
        app("fx", 4096, 1000, Some((0x2000_8000, 0x48080))),
    ];
    b.apply(Request {
        install: vec![NewApp {
            name: "fx".into(),
            candidates,
        }],
        ..Default::default()
    })
    .unwrap();
    let found = discover(&b.region, &b.flash()).unwrap();
    let (address, header) = found.apps().next().unwrap();
    assert_eq!(address, 0x48000);
    assert_eq!(address + header.size_before_app() as u64, 0x48080);
    // The gap before it is padding, so the kernel still finds it.
    assert!(matches!(
        found.entries[0],
        Entry::Padding {
            address: 0x40000,
            size: 0x8000
        }
    ));
}

#[test]
fn fixed_and_pic_apps_can_mix() {
    let mut b = Board::new(Placement::default());
    let fx = NewApp {
        name: "fx".into(),
        candidates: vec![app("fx", 4096, 1000, Some((0, 0x41080)))],
    };
    b.apply(Request {
        install: vec![fx, new_app("pic", 2048)],
        ..Default::default()
    })
    .unwrap();
    let apps = b.apps();
    assert_eq!(apps[0], ("pic".into(), 0x40000, 2048));
    assert_eq!(apps[1].1, 0x41000);
}

#[test]
fn discovery_asks_only_for_what_it_needs() {
    let mut b = Board::new(cortex_m());
    b.apply(install(&[("a", 8000), ("b", 8000)])).unwrap();
    let full = b.flash();
    let mut partial = Flash::new(&b.region);
    let mut bytes_read = 0u64;
    let mut rounds = 0usize;
    let found = loop {
        match discover(&b.region, &partial) {
            Ok(f) => break f,
            Err(NeedRead { ranges }) => {
                rounds += 1;
                for (address, len) in ranges {
                    bytes_read += len;
                    partial.insert(address, &full.read(address, len).unwrap());
                }
            }
        }
    };
    assert_eq!(found.apps().count(), 2);
    // Headers and the terminator, not pages around them, and nowhere near the
    // whole 256 KiB region.
    assert!(bytes_read < 1024, "read {bytes_read} bytes");
    // Bytes are not the only cost: each round is a device round trip, and over
    // openocd that is a dump_image and a temp file. One per app, plus the
    // terminator -- not two per app because the lengths and the header were
    // asked for separately.
    assert!(
        rounds <= found.apps().count() + 1,
        "{rounds} round trips for {} apps",
        found.apps().count()
    );
}

#[test]
fn an_identical_reinstall_writes_nothing() {
    let mut b = Board::new(cortex_m());
    b.apply(install(&[("a", 8000)])).unwrap();
    let (_, writes) = b.apply(install(&[("a", 8000)])).unwrap();
    assert!(writes.is_empty(), "{writes:?}");
}

#[test]
fn uninstalling_writes_only_the_header_it_rewrites() {
    let mut b = Board::new(cortex_m());
    b.apply(install(&[("a", 8000), ("b", 8000)])).unwrap();
    let (_, writes) = b
        .apply(Request {
            uninstall: vec!["a".into()],
            ..Default::default()
        })
        .unwrap();
    let total: usize = writes.iter().map(|w| w.data.len()).sum();
    assert!(total <= 64, "rewrote {total} bytes: {writes:?}");
}

#[test]
fn a_write_carries_its_bytes_and_not_the_pages_around_them() {
    // Adding a second app leaves the first alone and writes that app's bytes,
    // not those rounded up to a 4096-byte page. It writes fewer bytes than the
    // app even occupies: the padding to its power-of-two size is already
    // erased, so there is nothing to put there.
    let mut b = Board::new(cortex_m());
    b.apply(install(&[("a", 8000)])).unwrap();
    let (_, writes) = b.apply(install(&[("c", 2000)])).unwrap();
    let total: usize = writes.iter().map(|w| w.data.len()).sum();
    assert!(total > 1000 && total < 2048, "wrote {total} bytes");
}

#[test]
fn a_header_too_long_for_the_region_ends_the_list() {
    // A region smaller than a u16 header_size, so a corrupt header's claimed
    // length reaches past the end of flash. Reading it would ask the device
    // for bytes outside the region the board file describes.
    let region = Region {
        start: 0x40000,
        size: 1024,
    };
    let mut image = vec![0xFFu8; region.size as usize];
    // Version 2, header_size 0xFFFF, total_size 0xFFFF_FFFF: Lengths::parse
    // accepts it, since total >= header >= base.
    image[0..2].copy_from_slice(&2u16.to_le_bytes());
    image[2..4].copy_from_slice(&0xFFFFu16.to_le_bytes());
    image[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

    let flash = Flash::from_image(&region, region.start, &image);
    let found = discover(&region, &flash).expect("ends the list, does not read past it");
    assert_eq!(found.apps().count(), 0);
    assert_eq!(found.end(&region), region.start);
}

#[test]
fn trailing_slack_too_small_for_a_header_ends_the_list() {
    // A region whose tail is four bytes: not enough to read even the length
    // prefix of a header. The walk must stop cleanly rather than reading
    // short and parsing garbage.
    let tbf = tbf::testing::app("a", 2048, 1024, None);
    let region = Region {
        start: 0x40000,
        size: 2048 + 4,
    };
    let mut image = vec![0xFFu8; region.size as usize];
    let bytes = tbf.content_bytes();
    image[..bytes.len()].copy_from_slice(&bytes);

    let flash = Flash::from_image(&region, region.start, &image);
    let found = discover(&region, &flash).expect("stops cleanly on a short tail");
    assert_eq!(found.apps().count(), 1);
    assert_eq!(found.end(&region), region.start + 2048);
}

#[test]
fn an_app_list_that_exactly_fills_the_region_terminates() {
    // The lora-e5-mini case: the canonical app fills flash, so the walk ends
    // with the cursor exactly on region.end(). Upstream tockloader reads past
    // that point and the probe errors; stopping here is the whole reason the
    // board file states where flash ends.
    let tbf = tbf::testing::app("a", 4096, 2048, None);
    let region = Region {
        start: 0x40000,
        size: 4096,
    };
    let mut image = vec![0xFFu8; region.size as usize];
    let bytes = tbf.content_bytes();
    image[..bytes.len()].copy_from_slice(&bytes);

    let flash = Flash::from_image(&region, region.start, &image);
    let found = discover(&region, &flash).expect("terminates with no room left");
    assert_eq!(found.apps().count(), 1);
    assert_eq!(found.end(&region), region.end());
}
