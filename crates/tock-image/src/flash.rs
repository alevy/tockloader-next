use std::collections::BTreeMap;

use crate::Region;

/// What is known about the contents of flash, page by page. Pages that have
/// not been read are unknown, not erased.
#[derive(Clone, Debug)]
pub struct Flash {
    page_size: u64,
    pages: BTreeMap<u64, Vec<u8>>,
}

/// Pages the caller must read (and add with [`Flash::insert_page`]) before the
/// operation can continue.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("need to read {} page(s) starting at {:#x}", pages.len(), pages.first().copied().unwrap_or(0))]
pub struct NeedRead {
    /// Page-aligned addresses.
    pub pages: Vec<u64>,
}

impl Flash {
    pub fn new(region: &Region) -> Flash {
        Flash {
            page_size: region.page_size,
            pages: BTreeMap::new(),
        }
    }

    /// Everything known: e.g. a flash image file covering `base..`.
    pub fn from_image(region: &Region, base: u64, image: &[u8]) -> Flash {
        let mut f = Flash::new(region);
        f.insert(base, image);
        f
    }

    pub fn page_size(&self) -> u64 {
        self.page_size
    }

    pub fn insert_page(&mut self, address: u64, data: Vec<u8>) {
        assert_eq!(address % self.page_size, 0, "unaligned page");
        assert_eq!(data.len() as u64, self.page_size, "partial page");
        self.pages.insert(address, data);
    }

    /// Insert any page-aligned run of bytes; partial trailing pages are
    /// dropped.
    pub fn insert(&mut self, address: u64, data: &[u8]) {
        let ps = self.page_size as usize;
        for (i, chunk) in data.chunks(ps).enumerate() {
            if chunk.len() == ps {
                self.insert_page(address + (i * ps) as u64, chunk.to_vec());
            }
        }
    }

    pub fn page(&self, address: u64) -> Option<&[u8]> {
        self.pages.get(&address).map(Vec::as_slice)
    }

    pub fn pages_covering(&self, address: u64, len: u64) -> impl Iterator<Item = u64> {
        let ps = self.page_size;
        let first = address / ps * ps;
        let end = address + len;
        (first..end).step_by(ps as usize)
    }

    /// Read bytes if every page covering them is known.
    pub fn read(&self, address: u64, len: u64) -> Result<Vec<u8>, NeedRead> {
        let missing: Vec<u64> = self
            .pages_covering(address, len)
            .filter(|p| !self.pages.contains_key(p))
            .collect();
        if !missing.is_empty() {
            return Err(NeedRead { pages: missing });
        }
        let mut out = Vec::with_capacity(len as usize);
        for p in self.pages_covering(address, len) {
            let page = &self.pages[&p];
            let lo = address.max(p) - p;
            let hi = (address + len).min(p + self.page_size) - p;
            out.extend_from_slice(&page[lo as usize..hi as usize]);
        }
        Ok(out)
    }

    /// Apply page writes, so the known state matches the device after
    /// writing.
    pub fn apply(&mut self, writes: &[crate::PageWrite]) {
        for w in writes {
            self.insert_page(w.address, w.data.clone());
        }
    }
}
