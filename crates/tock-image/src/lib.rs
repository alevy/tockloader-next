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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListEnd {
    /// Ran into bytes that are not a header (normally erased flash).
    Terminated,
    /// The list fills the region exactly.
    RegionFull,
    /// A header claims to extend past the region. Everything from here on is
    /// ignored, as the kernel would.
    Overrun,
}

#[derive(Clone, Debug)]
pub struct Discovered {
    pub entries: Vec<Entry>,
    /// First address after the list.
    pub end: u64,
    pub how: ListEnd,
}

impl Discovered {
    pub fn apps(&self) -> impl Iterator<Item = (u64, &Header)> {
        self.entries.iter().filter_map(|e| match e {
            Entry::App { address, header } => Some((*address, header)),
            _ => None,
        })
    }
}

/// Walk the linked list of TBFs the way the kernel does.
pub fn discover(region: &Region, flash: &Flash) -> Result<Discovered, NeedRead> {
    let mut entries = Vec::new();
    let mut address = region.start;
    let how = loop {
        if address == region.end() {
            break ListEnd::RegionFull;
        }
        if address + 8 > region.end() {
            break ListEnd::Terminated;
        }
        // Ask for the whole header in one go, not 8 bytes and then the rest:
        // the two requests come from different calls, so nothing downstream
        // can merge them, and each one is a device round trip. The extra
        // bytes are nearly free; the round trip is not.
        let probe = COALESCE_GAP.min(region.end() - address);
        let head = flash.read(address, probe)?;
        let Some(lengths) = Lengths::parse(&head[..8]) else {
            break ListEnd::Terminated;
        };
        let total = lengths.total_size as u64;
        if address + total > region.end() {
            break ListEnd::Overrun;
        }
        let header_size = lengths.header_size as u64;
        let bytes = if header_size <= probe {
            head[..header_size as usize].to_vec()
        } else {
            flash.read(address, header_size)?
        };
        match Header::parse(&bytes) {
            Ok(header) if header.is_app() => entries.push(Entry::App { address, header }),
            Ok(_) => entries.push(Entry::Padding {
                address,
                size: total,
            }),
            Err(_) => break ListEnd::Terminated,
        }
        address += total;
    };
    Ok(Discovered {
        entries,
        end: address,
        how,
    })
}
