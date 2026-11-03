//! The pure core of app management.
//!
//! Nothing here touches hardware. The device-facing loop lives in the caller:
//!
//! 1. [`discover`] walks the app list using whatever flash contents are known.
//!    If it needs bytes it does not have, it returns [`NeedRead`] and the
//!    caller fetches those ranges and calls it again.
//! 2. [`plan`] decides where every app goes, given the apps found and a
//!    [`Request`]. It only needs headers.
//! 3. [`render`] turns a plan into the byte runs that differ from what is
//!    already there. It may ask for more bytes, to move an app or to tell
//!    whether a run would change anything.
//!
//! Because each step is a pure function of its inputs, the whole thing can be
//! tested against flash images without a board, and runs in WASM.

mod flash;
mod planner;
mod ranges;
mod render;

pub use flash::{Flash, NeedRead};
pub use planner::{Action, NewApp, Plan, PlanError, Request, Slot, plan};
pub use ranges::coalesce;

/// Changed runs closer together than this are merged into one write. Sending
/// a few extra bytes is far cheaper than a second erase cycle on a transport
/// that erases before writing.
pub const COALESCE_GAP: u64 = 256;
pub use render::{WriteRun, render};

use serde::{Deserialize, Serialize};
use tbf::{Header, Lengths};

/// The flash region that holds the app list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub start: u64,
    pub size: u64,
}

impl Region {
    pub fn end(&self) -> u64 {
        self.start + self.size
    }
}

/// Rules the kernel's memory protection imposes on where apps can go. These
/// come from the board file; nothing is inferred from the architecture.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Placement {
    #[serde(default)]
    pub order: Order,
    #[serde(default)]
    pub size: SizeRule,
    #[serde(default)]
    pub alignment: Alignment,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    #[default]
    AsGiven,
    SizeDescending,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeRule {
    #[default]
    Any,
    PowerOfTwo,
    Multiple(u64),
}

impl SizeRule {
    pub fn apply(&self, size: u64) -> u64 {
        match self {
            SizeRule::Any => size,
            SizeRule::PowerOfTwo => size.next_power_of_two(),
            SizeRule::Multiple(m) => size.div_ceil(*m) * m,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Alignment {
    #[default]
    None,
    /// Apps start at a multiple of their total size (Cortex-M MPU).
    Size,
}

/// One element of the app list in flash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    App { address: u64, header: Header },
    Padding { address: u64, size: u64 },
}

impl Entry {
    pub fn address(&self) -> u64 {
        match self {
            Entry::App { address, .. } | Entry::Padding { address, .. } => *address,
        }
    }
    pub fn size(&self) -> u64 {
        match self {
            Entry::App { header, .. } => header.total_size as u64,
            Entry::Padding { size, .. } => *size,
        }
    }
}

/// The apps and padding found in a region, in address order.
#[derive(Clone, Debug)]
pub struct Discovered {
    pub entries: Vec<Entry>,
}

impl Discovered {
    pub fn apps(&self) -> impl Iterator<Item = (u64, &Header)> {
        self.entries.iter().filter_map(|e| match e {
            Entry::App { address, header } => Some((*address, header)),
            _ => None,
        })
    }

    /// First address after the list. Not derivable from [`Self::apps`] alone,
    /// since padding entries count towards it too.
    pub fn end(&self, region: &Region) -> u64 {
        self.entries
            .last()
            .map_or(region.start, |e| e.address() + e.size())
    }
}

/// How much to read when probing for a header. A TBF header is well under
/// this, so one read covers the lengths and the header together rather than
/// paying for two round trips. A format fact, not a transport one.
const HEADER_PROBE: u64 = 256;

/// Read the header at `address`, or `None` if there is not one there.
///
/// Reads [`HEADER_PROBE`] bytes speculatively: the length prefix comes from
/// the start of them and the header almost always lies inside the rest, so
/// one read covers both.
fn read_header(region: &Region, flash: &Flash, address: u64) -> Result<Option<Header>, NeedRead> {
    let available = region.end().saturating_sub(address);
    let probe = HEADER_PROBE.min(available);
    let head = flash.read(address, probe)?;
    // Unsliced: `Lengths::parse` returns `None` when there are too few bytes,
    // so this is safe on a region whose tail is shorter than a length prefix,
    // rather than relying on the caller having checked.
    let Some(lengths) = Lengths::parse(&head) else {
        return Ok(None);
    };
    let header_size = lengths.header_size as u64;
    // A header claiming to be longer than the region is not a header. Reading
    // it would ask the device for bytes outside the region the board file
    // describes, which is the thing this walk exists not to do.
    if header_size > available {
        return Ok(None);
    }
    let bytes = if header_size <= probe {
        head[..header_size as usize].to_vec()
    } else {
        flash.read(address, header_size)?
    };
    Ok(Header::parse(&bytes).ok())
}

/// Walk the linked list of TBFs the way the kernel does.
pub fn discover(region: &Region, flash: &Flash) -> Result<Discovered, NeedRead> {
    let mut entries = Vec::new();
    let mut address = region.start;
    // `read_header` is what stops this at the end of the region -- it reads
    // nothing once the cursor reaches it, and nothing parses as a header. The
    // board file is what says where that end is, which is the case upstream
    // tockloader cannot express.
    while let Some(header) = read_header(region, flash, address)? {
        let total = header.total_size as u64;
        // A header claiming to run past the region ends the list, as it would
        // for the kernel.
        if address + total > region.end() {
            break;
        }
        entries.push(if header.is_app() {
            Entry::App { address, header }
        } else {
            Entry::Padding {
                address,
                size: total,
            }
        });
        address += total;
    }
    Ok(Discovered { entries })
}
