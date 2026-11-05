//! Tock Binary Format (TBF) and Tock Application Bundle (TAB) handling.
//!
//! This crate does no I/O: everything works on byte slices so it builds for
//! WASM and WASI targets.

mod header;
mod tab;

pub use header::{
    BASE_HEADER_SIZE, FLAG_ENABLED, FLAG_STICKY, Header, Lengths, NO_FIXED_ADDRESS, Tlv, checksum,
    tlv_type,
};
pub use tab::{Tab, TabTbf};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("not a TBF version 2 header")]
    NotAHeader,
    #[error("header truncated: need {needed} bytes, have {available}")]
    Truncated { needed: usize, available: usize },
    #[error("malformed TLV {tlv_type:#x} at header offset {offset}")]
    MalformedTlv { tlv_type: u16, offset: usize },
    #[error("app fixed to flash {fixed_flash:#x} cannot start at {address:#x}")]
    CannotPlaceFixed { address: u32, fixed_flash: u32 },
    #[error("TBF `{name}` is {len} bytes but its header says total_size is {total_size}")]
    TbfTooLong {
        name: String,
        len: usize,
        total_size: u32,
    },
    #[error("invalid TAB: {0}")]
    InvalidTab(String),
}

/// A complete TBF: the header plus everything after it (app binary and
/// footers), as found in a TAB or in flash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tbf {
    pub header: Header,
    /// Bytes from the end of the header up to at most `total_size`.
    pub after_header: Vec<u8>,
}

impl Tbf {
    pub fn parse(bytes: &[u8]) -> Result<Tbf, Error> {
        let header = Header::parse(bytes)?;
        let end = (header.total_size as usize).min(bytes.len());
        Ok(Tbf {
            after_header: bytes[header.header_size as usize..end].to_vec(),
            header,
        })
    }

    pub fn total_size(&self) -> u32 {
        self.header.total_size
    }

    /// Exactly `total_size` bytes: header, contents, then `fill` for any space
    /// reserved by `total_size` but not covered by the contents.
    ///
    /// If the header grew (for example to place a fixed-address app), the
    /// protected region absorbs it: the app binary keeps its absolute offset
    /// from the end of the protected region.
    pub fn to_bytes(&self, fill: u8) -> Vec<u8> {
        let header = self.header.to_bytes();
        let mut out = header;
        out.extend_from_slice(&self.after_header);
        out.resize(self.header.total_size as usize, fill);
        out
    }

    /// The header and contents, without filling the rest of `total_size`.
    /// What is in flash after the contents does not matter.
    pub fn content_bytes(&self) -> Vec<u8> {
        let mut out = self.header.to_bytes();
        out.extend_from_slice(&self.after_header);
        out.truncate(self.header.total_size as usize);
        out
    }

    /// Change `total_size`, e.g. to round up to a power of two.
    pub fn set_total_size(&mut self, size: u32) {
        self.header.total_size = size;
    }

    /// Fixed-address placement: grow the protected region so the binary lands
    /// at its fixed flash address when this TBF starts at `address`. The
    /// growth is inserted (as zeros) at the start of `after_header`, so the
    /// app binary itself moves to the right offset.
    ///
    /// Unlike Python tockloader, this also moves `binary_end_offset` so the
    /// kernel still sees the whole binary. It refuses to cut off anything but
    /// erased or zero bytes at the end to make room.
    pub fn place_fixed_at(&mut self, address: u32) -> Result<(), Error> {
        let before = self.header.size_before_app();
        self.header.place_fixed_at(address)?;
        let grown = (self.header.size_before_app() - before) as usize;
        if grown > 0 {
            let mut v = vec![0u8; grown];
            v.extend_from_slice(&self.after_header);
            let room = self.header.total_size as usize - self.header.header_size as usize;
            if v.len() > room && v[room..].iter().any(|b| *b != 0 && *b != 0xFF) {
                return Err(Error::CannotPlaceFixed {
                    address,
                    fixed_flash: self.header.fixed_addresses().1.unwrap_or(0),
                });
            }
            v.truncate(room.min(v.len()));
            self.after_header = v;
            if let Some(Tlv::Program {
                binary_end_offset, ..
            }) = self
                .header
                .tlvs
                .iter_mut()
                .find(|t| matches!(t, Tlv::Program { .. }))
            {
                *binary_end_offset += grown as u32;
            }
        }
        Ok(())
    }
}

/// Builders for synthetic TBFs, for tests in this and downstream crates.
#[doc(hidden)]
pub mod testing {
    use crate::{FLAG_ENABLED, Header, Tbf, Tlv};

    /// An app shaped like elf2tab output: Program, PackageName and
    /// (optionally) FixedAddresses TLVs, a recognizable binary, and
    /// `total_size` reserved.
    pub fn app(name: &str, total_size: u32, binary_len: u32, fixed: Option<(u32, u32)>) -> Tbf {
        let mut tlvs = vec![
            Tlv::Program {
                init_fn_offset: 0x41,
                protected_size: 0,
                minimum_ram_size: 4096,
                binary_end_offset: 0, // set below
                version: 1,
            },
            Tlv::PackageName(name.to_string()),
        ];
        if let Some((ram, flash)) = fixed {
            tlvs.push(Tlv::FixedAddresses { ram, flash });
        }
        let mut header = Header {
            header_size: 0,
            total_size,
            flags: FLAG_ENABLED,
            checksum: 0,
            tlvs,
        };
        header.header_size = header.to_bytes().len() as u16;
        let end = header.header_size as u32 + binary_len;
        if let Some(Tlv::Program {
            binary_end_offset, ..
        }) = header.tlvs.first_mut()
        {
            *binary_end_offset = end;
        }
        header.checksum = header.compute_checksum();
        let seed = name.bytes().fold(0u8, |a, b| a.wrapping_add(b));
        let after_header = (0..binary_len)
            .map(|i| seed.wrapping_add(i as u8))
            .collect();
        Tbf {
            header,
            after_header,
        }
    }
}

#[cfg(test)]
mod tests;
