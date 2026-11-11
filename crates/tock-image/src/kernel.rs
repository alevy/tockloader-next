//! What the kernel says about itself.
//!
//! Every Tock kernel leaves a table of attributes at the very end of its flash
//! region, for exactly this purpose -- the linker script that emits it says it
//! is "for host-side tools to learn about the installed kernel". A board file
//! is a declaration written by hand; this is the kernel in front of us, and
//! where the two disagree this is the one that is right.
//!
//! The format is specified in the Tock book, `doc/kernel_attributes.md`. It
//! is a TLV chain that grows *downwards* from the end of the region, under a
//! four-byte header and a `TOCK` sentinel:
//!
//! ```text
//!  <- lower addresses                                   higher addresses ->
//!  ... | Value | Type u16 | Length u16 | Reserved[3] | Version | T O C K
//!      <------------ one TLV ---------> <------ header -------> <sentinel>
//! ```
//!
//! `Length` counts the value only, so one step down is `4 + Length` bytes.
//!
//! # Finding it
//!
//! The table carries no length, no count and no end marker, and it is not
//! self-locating: you have to know where it ends before you can read it. The
//! book is explicit that tools "find the attributes by starting at the start
//! of userspace applications in flash and working backwards", so the app
//! region's start is an input here, not something this can discover.
//!
//! That is still worth something. A sentinel where one is expected *confirms*
//! the board file's app address -- the single value everything else depends
//! on -- and its absence is a reason to doubt it.
//!
//! # Where it stops
//!
//! With no end marker, the walk has to stop on the shape of what it reads.
//! The bytes below the table are whatever programming left there: `0x00` when
//! the kernel was flashed from a `.bin` (objcopy fills the gap) and `0xFF`
//! when it was flashed from the `.elf`, since the attributes are their own
//! load segment with an erased gap below them. Rather than test for either
//! pattern -- there is no reason to think those are the only two -- the walk
//! stops when a TLV cannot be a TLV: no room for a header, a zero `Length`
//! that could not advance, or a `Length` reaching past the floor. `0x00`
//! padding trips the second, `0xFF` padding the third.
//!
//! Unknown types are *skipped* by their `Length`, not treated as the end.
//! Python tockloader stops at the first type it does not recognise, which is
//! safe for the TLVs it writes itself -- it puts those furthest from the
//! sentinel -- but the kernel emits the app memory region *nearest* the
//! sentinel, so a TLV added upstream would be met first and would hide the
//! one value we most need behind a type we have never seen.

use crate::{Flash, NeedRead, Region};

/// The sentinel, at the four highest addresses of the kernel region.
const SENTINEL: &[u8; 4] = b"TOCK";

/// Sentinel, version byte and three reserved bytes.
const HEADER: u64 = 8;

/// A TLV's type and length, which sit above its value.
const TLV_HEADER: u64 = 4;

/// How far below the app region a table may reach before we stop believing
/// in it. The real one is 44 bytes; Python tockloader reads a 1000-byte
/// window. Without a length field in the format this is a guess, and its job
/// is only to bound a walk through bytes that may be nothing at all.
pub const MAX_TABLE: u64 = 1024;

/// The format this understands. A kernel announcing anything else is still
/// parsed -- the framing has outlived one version bump already and refusing
/// outright would make a newer kernel unreadable for no safety gained -- but
/// the version is reported so a caller can say so.
pub const VERSION: u8 = 1;

/// The Tock kernel's own version, from [`tlv::KERNEL_VERSION`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
    /// 0 for a release. 1 is a working development tree; 2 and up are alpha,
    /// beta and so on.
    pub prerelease: u16,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Version {
            major,
            minor,
            patch,
            prerelease,
        } = self;
        write!(f, "{major}.{minor}.{patch}")?;
        match prerelease {
            0 => Ok(()),
            1 => write!(f, "-dev"),
            2 => write!(f, "-alpha"),
            3 => write!(f, "-beta"),
            n => write!(f, "-pre{n}"),
        }
    }
}

/// TLV types, from the book's registry. A separate namespace from TBF's TLV
/// types, which the book asks implementations not to let collide.
pub mod tlv {
    pub const APP_MEMORY: u16 = 0x0101;
    pub const KERNEL_BINARY: u16 = 0x0102;
    pub const KERNEL_VERSION: u16 = 0x0103;
}

/// A TLV this crate does not model, kept as it was found.
///
/// Not discarded, for the same reason [`tbf::Tlv::Raw`] is not: something we
/// do not understand is not the same as something that is not there, and a
/// caller reporting what a kernel said should be able to say "and three more
/// I could not read".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawTlv {
    pub tlv_type: u16,
    pub value: Vec<u8>,
}

/// What a kernel's attribute table says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attributes {
    /// The format version, which may not be one this knows.
    pub version: u8,
    /// RAM the kernel has set aside for apps. The board file's `app_ram` is a
    /// guess at this and is generally too generous: the kernel's own data and
    /// bss come out of the bottom of it, and how much depends on the build.
    pub app_ram: Option<Region>,
    /// The kernel *binary* -- not the region reserved for it, which is larger
    /// and is where this table lives.
    pub kernel_binary: Option<Region>,
    pub kernel_version: Option<Version>,
    /// Types this does not model, in the order they were met walking down.
    pub other: Vec<RawTlv>,
}

impl Attributes {
    /// Whether this is a format this fully understands.
    pub fn understood(&self) -> bool {
        self.version == VERSION
    }
}

/// Read the table at the end of `window`.
///
/// `window` is the bytes immediately below the end of the kernel region, in
/// flash order, so the sentinel is its last four bytes. `None` means there is
/// no table there, which is not an error -- a board may be running a kernel
/// older than the format, or nothing at all.
///
/// Pure, and takes a slice rather than a [`Flash`], so it can be run against
/// a kernel image on disk as easily as against a device.
pub fn parse(window: &[u8]) -> Option<Attributes> {
    if window.len() < HEADER as usize {
        return None;
    }
    let n = window.len();
    if &window[n - 4..] != SENTINEL {
        return None;
    }
    let version = window[n - 5];

    let mut attributes = Attributes {
        version,
        app_ram: None,
        kernel_binary: None,
        kernel_version: None,
        other: Vec::new(),
    };

    // Everything below the header is TLVs, until one cannot be one.
    let header = TLV_HEADER as usize;
    let floor = n.saturating_sub(MAX_TABLE as usize);
    let mut at = n - HEADER as usize;
    while at >= floor + header {
        let tlv_type = u16::from_le_bytes([window[at - 4], window[at - 3]]);
        let length = u16::from_le_bytes([window[at - 2], window[at - 1]]) as usize;
        // A TLV with no value cannot advance the walk, and means nothing in
        // this format. Zeroed padding below the table lands here.
        if length == 0 {
            break;
        }
        // A value reaching past the floor is not a value. Erased padding
        // claims 65535 bytes and lands here.
        if at - header < floor + length {
            break;
        }
        attributes.take(tlv_type, &window[at - header - length..at - header]);
        at -= header + length;
    }
    Some(attributes)
}

impl Attributes {
    /// Record one TLV. A type we know but whose length is wrong is kept as
    /// raw rather than guessed at -- the alternative is inventing an address
    /// from the wrong number of bytes.
    fn take(&mut self, tlv_type: u16, value: &[u8]) {
        match (tlv_type, value.len()) {
            (tlv::APP_MEMORY, 8) => self.app_ram = Some(region(value)),
            (tlv::KERNEL_BINARY, 8) => self.kernel_binary = Some(region(value)),
            (tlv::KERNEL_VERSION, 8) => {
                let half = |i: usize| u16::from_le_bytes([value[i * 2], value[i * 2 + 1]]);
                self.kernel_version = Some(Version {
                    major: half(0),
                    minor: half(1),
                    patch: half(2),
                    prerelease: half(3),
                })
            }
            _ => self.other.push(RawTlv {
                tlv_type,
                value: value.to_vec(),
            }),
        }
    }
}

impl Attributes {
    /// The bytes of this table, in flash order.
    ///
    /// Emitted in the kernel's own order: version lowest, then the binary,
    /// then app memory nearest the sentinel, with anything unmodelled below
    /// them all. A canonical table survives `parse(&to_bytes(..))` unchanged.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut tlv = |tlv_type: u16, value: &[u8]| {
            out.extend_from_slice(value);
            out.extend_from_slice(&tlv_type.to_le_bytes());
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        };
        for raw in self.other.iter().rev() {
            tlv(raw.tlv_type, &raw.value);
        }
        if let Some(v) = self.kernel_version {
            let mut value = Vec::new();
            for half in [v.major, v.minor, v.patch, v.prerelease] {
                value.extend_from_slice(&half.to_le_bytes());
            }
            tlv(tlv::KERNEL_VERSION, &value);
        }
        if let Some(r) = self.kernel_binary {
            tlv(tlv::KERNEL_BINARY, &bytes_of(r));
        }
        if let Some(r) = self.app_ram {
            tlv(tlv::APP_MEMORY, &bytes_of(r));
        }
        out.extend_from_slice(&[0, 0, 0, self.version]);
        out.extend_from_slice(SENTINEL);
        out
    }
}

/// A region as the two little-endian words a TLV carries.
fn bytes_of(region: Region) -> Vec<u8> {
    let mut v = (region.start as u32).to_le_bytes().to_vec();
    v.extend_from_slice(&(region.size as u32).to_le_bytes());
    v
}

/// Two little-endian 32-bit words: a start and a length.
fn region(value: &[u8]) -> Region {
    let word = |i: usize| {
        u32::from_le_bytes([
            value[i * 4],
            value[i * 4 + 1],
            value[i * 4 + 2],
            value[i * 4 + 3],
        ]) as u64
    };
    Region {
        start: word(0),
        size: word(1),
    }
}

/// The bytes a table ending at `end` could occupy, bounded below by `floor`.
///
/// Separate from [`attributes`] because the caller wants it too: the window
/// abuts the app region, so one read can serve both this and the walk that
/// follows it.
pub fn window(end: u64, floor: u64) -> Option<Region> {
    let start = end.saturating_sub(MAX_TABLE).max(floor);
    let size = end.checked_sub(start)?;
    // Too small to hold a sentinel and one TLV is too small to bother a
    // device about.
    (size >= HEADER + TLV_HEADER).then_some(Region { start, size })
}

/// Read the table ending at `end`, asking for bytes as [`discover`] does.
///
/// `floor` is the lowest address the caller may read -- the start of flash,
/// or of the kernel region. Returns `Ok(None)` when there is no table, or no
/// room for one.
///
/// [`discover`]: crate::discover
pub fn attributes(end: u64, floor: u64, flash: &Flash) -> Result<Option<Attributes>, NeedRead> {
    let Some(window) = window(end, floor) else {
        return Ok(None);
    };
    let bytes = flash.read(window.start, window.size)?;
    Ok(parse(&bytes))
}

#[cfg(test)]
#[path = "kernel/tests.rs"]
mod tests;
