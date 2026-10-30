//! TBF version 2 headers.
//!
//! The parser keeps every TLV it does not model as raw bytes, so a header that
//! is parsed and re-serialized without modification is byte-for-byte identical.

use crate::Error;

pub const BASE_HEADER_SIZE: usize = 16;

pub const FLAG_ENABLED: u32 = 0x1;
pub const FLAG_STICKY: u32 = 0x2;

/// Value used in the FixedAddresses TLV for "no fixed address".
pub const NO_FIXED_ADDRESS: u32 = 0xFFFF_FFFF;

pub mod tlv_type {
    pub const MAIN: u16 = 0x01;
    pub const WRITEABLE_FLASH_REGIONS: u16 = 0x02;
    pub const PACKAGE_NAME: u16 = 0x03;
    pub const PIC_OPTION_1: u16 = 0x04;
    pub const FIXED_ADDRESSES: u16 = 0x05;
    pub const PERMISSIONS: u16 = 0x06;
    pub const PERSISTENT_ACL: u16 = 0x07;
    pub const KERNEL_VERSION: u16 = 0x08;
    pub const PROGRAM: u16 = 0x09;
    pub const SHORT_ID: u16 = 0x0A;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tlv {
    Main {
        init_fn_offset: u32,
        protected_size: u32,
        minimum_ram_size: u32,
    },
    Program {
        init_fn_offset: u32,
        protected_size: u32,
        minimum_ram_size: u32,
        binary_end_offset: u32,
        version: u32,
    },
    PackageName(String),
    FixedAddresses {
        ram: u32,
        flash: u32,
    },
    KernelVersion {
        major: u16,
        minor: u16,
    },
    ShortId(u32),
    /// Any TLV this crate does not interpret, kept verbatim (unpadded).
    Raw {
        tlv_type: u16,
        data: Vec<u8>,
    },
}

impl Tlv {
    pub fn tlv_type(&self) -> u16 {
        match self {
            Tlv::Main { .. } => tlv_type::MAIN,
            Tlv::Program { .. } => tlv_type::PROGRAM,
            Tlv::PackageName(_) => tlv_type::PACKAGE_NAME,
            Tlv::FixedAddresses { .. } => tlv_type::FIXED_ADDRESSES,
            Tlv::KernelVersion { .. } => tlv_type::KERNEL_VERSION,
            Tlv::ShortId(_) => tlv_type::SHORT_ID,
            Tlv::Raw { tlv_type, .. } => *tlv_type,
        }
    }

    fn value_bytes(&self) -> Vec<u8> {
        let words = |ws: &[u32]| ws.iter().flat_map(|w| w.to_le_bytes()).collect();
        match self {
            Tlv::Main {
                init_fn_offset,
                protected_size,
                minimum_ram_size,
            } => words(&[*init_fn_offset, *protected_size, *minimum_ram_size]),
            Tlv::Program {
                init_fn_offset,
                protected_size,
                minimum_ram_size,
                binary_end_offset,
                version,
            } => words(&[
                *init_fn_offset,
                *protected_size,
                *minimum_ram_size,
                *binary_end_offset,
                *version,
            ]),
            Tlv::PackageName(name) => name.as_bytes().to_vec(),
            Tlv::FixedAddresses { ram, flash } => words(&[*ram, *flash]),
            Tlv::KernelVersion { major, minor } => {
                let mut v = major.to_le_bytes().to_vec();
                v.extend_from_slice(&minor.to_le_bytes());
                v
            }
            Tlv::ShortId(id) => words(&[*id]),
            Tlv::Raw { data, .. } => data.clone(),
        }
    }

    fn parse(tlv_type: u16, data: &[u8]) -> Tlv {
        let word = |i: usize| u32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap());
        let raw = || Tlv::Raw {
            tlv_type,
            data: data.to_vec(),
        };
        match (tlv_type, data.len()) {
            (tlv_type::MAIN, 12) => Tlv::Main {
                init_fn_offset: word(0),
                protected_size: word(1),
                minimum_ram_size: word(2),
            },
            (tlv_type::PROGRAM, 20) => Tlv::Program {
                init_fn_offset: word(0),
                protected_size: word(1),
                minimum_ram_size: word(2),
                binary_end_offset: word(3),
                version: word(4),
            },
            (tlv_type::PACKAGE_NAME, _) => match std::str::from_utf8(data) {
                Ok(s) => Tlv::PackageName(s.to_string()),
                Err(_) => raw(),
            },
            (tlv_type::FIXED_ADDRESSES, 8) => Tlv::FixedAddresses {
                ram: word(0),
                flash: word(1),
            },
            (tlv_type::KERNEL_VERSION, 4) => Tlv::KernelVersion {
                major: u16::from_le_bytes([data[0], data[1]]),
                minor: u16::from_le_bytes([data[2], data[3]]),
            },
            (tlv_type::SHORT_ID, 4) => Tlv::ShortId(word(0)),
            _ => raw(),
        }
    }
}

/// A parsed TBF version 2 header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub header_size: u16,
    pub total_size: u32,
    pub flags: u32,
    /// Checksum as stored. Recomputed by [`Header::to_bytes`].
    pub checksum: u32,
    pub tlvs: Vec<Tlv>,
}

/// Lengths that can be read from the first 8 bytes of a header without parsing
/// the rest. This is all that is needed to walk the list of apps in flash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lengths {
    pub version: u16,
    pub header_size: u16,
    pub total_size: u32,
}

impl Lengths {
    /// Returns `None` if `bytes` does not start with a plausible version 2
    /// header (e.g. erased flash).
    pub fn parse(bytes: &[u8]) -> Option<Lengths> {
        if bytes.len() < 8 {
            return None;
        }
        let version = u16::from_le_bytes([bytes[0], bytes[1]]);
        let header_size = u16::from_le_bytes([bytes[2], bytes[3]]);
        let total_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if version != 2
            || (header_size as usize) < BASE_HEADER_SIZE
            || total_size < header_size as u32
        {
            return None;
        }
        Some(Lengths {
            version,
            header_size,
            total_size,
        })
    }
}

impl Header {
    /// A header with no TLVs, which the kernel skips over. Used to fill gaps
    /// between apps so the list of apps in flash stays contiguous.
    pub fn padding(total_size: u32) -> Header {
        let mut h = Header {
            header_size: BASE_HEADER_SIZE as u16,
            total_size,
            flags: 0,
            checksum: 0,
            tlvs: vec![],
        };
        h.checksum = h.compute_checksum();
        h
    }

    /// Parse a header from the start of `bytes`. `bytes` must contain at least
    /// `header_size` bytes.
    pub fn parse(bytes: &[u8]) -> Result<Header, Error> {
        let lengths = Lengths::parse(bytes).ok_or(Error::NotAHeader)?;
        let header_size = lengths.header_size as usize;
        if bytes.len() < header_size {
            return Err(Error::Truncated {
                needed: header_size,
                available: bytes.len(),
            });
        }
        let flags = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let checksum = u32::from_le_bytes(bytes[12..16].try_into().unwrap());

        let mut tlvs = Vec::new();
        let mut offset = BASE_HEADER_SIZE;
        while offset + 4 <= header_size {
            let tlv_type = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
            let len = u16::from_le_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
            offset += 4;
            if offset + len > header_size {
                return Err(Error::MalformedTlv { tlv_type, offset });
            }
            tlvs.push(Tlv::parse(tlv_type, &bytes[offset..offset + len]));
            offset += round_up4(len);
        }

        let header = Header {
            header_size: lengths.header_size,
            total_size: lengths.total_size,
            flags,
            checksum,
            tlvs,
        };
        Ok(header)
    }

    /// Serialize, recomputing the checksum. The output is `header_size` bytes
    /// long; if the TLVs need more room than `header_size`, `header_size` is
    /// grown first.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.to_bytes_unchecked();
        let checksum = checksum(&out);
        out[12..16].copy_from_slice(&checksum.to_le_bytes());
        out
    }

    fn to_bytes_unchecked(&self) -> Vec<u8> {
        let mut body = Vec::new();
        for tlv in &self.tlvs {
            let value = tlv.value_bytes();
            body.extend_from_slice(&tlv.tlv_type().to_le_bytes());
            body.extend_from_slice(&(value.len() as u16).to_le_bytes());
            body.extend_from_slice(&value);
            body.resize(round_up4(body.len()), 0);
        }
        let header_size = (BASE_HEADER_SIZE + body.len()).max(self.header_size as usize);
        let mut out = Vec::with_capacity(header_size);
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&(header_size as u16).to_le_bytes());
        out.extend_from_slice(&self.total_size.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&body);
        out.resize(header_size, 0);
        out
    }

    pub fn compute_checksum(&self) -> u32 {
        checksum(&self.to_bytes_unchecked())
    }

    pub fn checksum_ok(&self) -> bool {
        self.checksum == self.compute_checksum()
    }

    /// Apps have a Main or Program TLV; anything else is padding.
    pub fn is_app(&self) -> bool {
        self.program_tlv().is_some()
    }

    pub fn is_enabled(&self) -> bool {
        self.flags & FLAG_ENABLED != 0
    }

    pub fn is_sticky(&self) -> bool {
        self.flags & FLAG_STICKY != 0
    }

    pub fn package_name(&self) -> Option<&str> {
        self.tlvs.iter().find_map(|t| match t {
            Tlv::PackageName(n) => Some(n.as_str()),
            _ => None,
        })
    }

    pub fn app_version(&self) -> Option<u32> {
        self.tlvs.iter().find_map(|t| match t {
            Tlv::Program { version, .. } => Some(*version),
            _ => None,
        })
    }

    pub fn kernel_version(&self) -> Option<(u16, u16)> {
        self.tlvs.iter().find_map(|t| match t {
            Tlv::KernelVersion { major, minor } => Some((*major, *minor)),
            _ => None,
        })
    }

    /// `(ram, flash)`, each `None` if not fixed. The kernel treats
    /// `0xFFFFFFFF` as "not fixed".
    pub fn fixed_addresses(&self) -> (Option<u32>, Option<u32>) {
        let fixed = |v: u32| (v != NO_FIXED_ADDRESS).then_some(v);
        self.tlvs
            .iter()
            .find_map(|t| match t {
                Tlv::FixedAddresses { ram, flash } => Some((fixed(*ram), fixed(*flash))),
                _ => None,
            })
            .unwrap_or((None, None))
    }

    /// `(init_fn_offset, protected_size)` from the Program TLV, or the Main
    /// TLV if there is no Program TLV.
    fn program_tlv(&self) -> Option<(u32, u32)> {
        let program = self.tlvs.iter().find_map(|t| match t {
            Tlv::Program {
                init_fn_offset,
                protected_size,
                ..
            } => Some((*init_fn_offset, *protected_size)),
            _ => None,
        });
        program.or_else(|| {
            self.tlvs.iter().find_map(|t| match t {
                Tlv::Main {
                    init_fn_offset,
                    protected_size,
                    ..
                } => Some((*init_fn_offset, *protected_size)),
                _ => None,
            })
        })
    }

    /// Bytes from the start of the TBF to the start of the app binary.
    pub fn size_before_app(&self) -> u32 {
        self.header_size as u32 + self.program_tlv().map_or(0, |(_, p)| p)
    }

    /// Offset where the app binary ends and footers begin.
    pub fn binary_end_offset(&self) -> u32 {
        self.tlvs
            .iter()
            .find_map(|t| match t {
                Tlv::Program {
                    binary_end_offset, ..
                } => Some(*binary_end_offset),
                _ => None,
            })
            .unwrap_or(self.total_size)
    }

    /// Grow the protected region so that, with the TBF starting at `address`,
    /// the app binary lands on its fixed flash address. Mirrors
    /// tockloader's `adjust_starting_address`, including also moving
    /// `init_fn_offset`, which the kernel measures from the end of the header.
    pub fn place_fixed_at(&mut self, address: u32) -> Result<(), Error> {
        let Some(fixed_flash) = self.fixed_addresses().1 else {
            return Ok(());
        };
        let current = address as u64 + self.size_before_app() as u64;
        if current == fixed_flash as u64 {
            return Ok(());
        }
        if current > fixed_flash as u64 {
            return Err(Error::CannotPlaceFixed {
                address,
                fixed_flash,
            });
        }
        let delta = (fixed_flash as u64 - current) as u32;
        for tlv in self.tlvs.iter_mut() {
            match tlv {
                Tlv::Program {
                    init_fn_offset,
                    protected_size,
                    ..
                }
                | Tlv::Main {
                    init_fn_offset,
                    protected_size,
                    ..
                } => {
                    *protected_size += delta;
                    *init_fn_offset += delta;
                    return Ok(());
                }
                _ => {}
            }
        }
        Err(Error::CannotPlaceFixed {
            address,
            fixed_flash,
        })
    }
}

/// XOR of the header's 32-bit little-endian words with the checksum field
/// zeroed.
pub fn checksum(header_bytes: &[u8]) -> u32 {
    let mut sum = 0u32;
    for (i, chunk) in header_bytes.chunks(4).enumerate() {
        if i == 3 {
            continue; // the checksum field itself
        }
        let mut w = [0u8; 4];
        w[..chunk.len()].copy_from_slice(chunk);
        sum ^= u32::from_le_bytes(w);
    }
    sum
}

pub(crate) fn round_up4(n: usize) -> usize {
    (n + 3) & !3
}
