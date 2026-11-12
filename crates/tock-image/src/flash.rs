use std::collections::BTreeMap;

/// What is known about the contents of flash. Ranges that have not been read
/// are unknown, not erased. Nothing here models erase geometry: that belongs
/// to whichever transport is on the other end.
///
/// Deliberately unbounded. This took a [`Region`] once and ignored it, and
/// the obvious repair -- refuse anything outside -- would have been the
/// wrong one twice over. It is a record of bytes somebody read, not a say in
/// which bytes they may; the address a caller is allowed to touch is the
/// transport's to rule on, since a transport is an executable and anything
/// in front of it can be gone around. A second opinion here would be the
/// `Bounded` decorator again: a shared guard that made a transport missing
/// its own check look correct.
///
/// [`Region`]: crate::Region
#[derive(Clone, Debug, Default)]
pub struct Flash {
    /// Non-overlapping, non-abutting known runs, keyed by start address.
    runs: BTreeMap<u64, Vec<u8>>,
}

/// Byte ranges the caller must read (and add with [`Flash::insert`]) before
/// the operation can continue.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("need to read {} range(s) starting at {:#x}", ranges.len(), ranges.first().map(|r| r.0).unwrap_or(0))]
pub struct NeedRead {
    /// `(address, len)` pairs.
    pub ranges: Vec<(u64, u64)>,
}

impl Flash {
    /// Nothing known yet.
    pub fn new() -> Flash {
        Flash::default()
    }

    /// Record known bytes, merging with anything they touch or abut.
    pub fn insert(&mut self, address: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let end = address + data.len() as u64;
        // Collect every run that overlaps or abuts [address, end), widen to
        // cover them all, then replace them with one run.
        let overlapping: Vec<u64> = self
            .runs
            .range(..=end)
            .filter(|(start, run)| **start + run.len() as u64 >= address)
            .map(|(start, _)| *start)
            .collect();
        let mut start = address;
        let mut stop = end;
        for s in &overlapping {
            start = start.min(*s);
            stop = stop.max(*s + self.runs[s].len() as u64);
        }
        let mut merged = vec![0u8; (stop - start) as usize];
        for s in &overlapping {
            let run = &self.runs[s];
            let at = (*s - start) as usize;
            merged[at..at + run.len()].copy_from_slice(run);
        }
        let at = (address - start) as usize;
        merged[at..at + data.len()].copy_from_slice(data);
        for s in overlapping {
            self.runs.remove(&s);
        }
        self.runs.insert(start, merged);
    }

    /// Read bytes if every one of them is known.
    pub fn read(&self, address: u64, len: u64) -> Result<Vec<u8>, NeedRead> {
        if len == 0 {
            return Ok(Vec::new());
        }
        match self.known(address, len) {
            Some(bytes) => Ok(bytes),
            None => Err(NeedRead {
                ranges: self.missing(address, len),
            }),
        }
    }

    /// The bytes, if the whole span sits inside one known run.
    fn known(&self, address: u64, len: u64) -> Option<Vec<u8>> {
        let (start, run) = self.runs.range(..=address).next_back()?;
        let offset = (address - start) as usize;
        let end = offset + len as usize;
        (end <= run.len()).then(|| run[offset..end].to_vec())
    }

    /// The sub-ranges of `[address, address + len)` that are not known.
    fn missing(&self, address: u64, len: u64) -> Vec<(u64, u64)> {
        let mut gaps = Vec::new();
        let mut at = address;
        let end = address + len;
        while at < end {
            match self
                .runs
                .range(..=at)
                .next_back()
                .filter(|(s, r)| **s + r.len() as u64 > at)
            {
                // Known here; skip to the end of this run.
                Some((s, r)) => at = s + r.len() as u64,
                None => {
                    // Unknown until the next run begins, or the end.
                    let next = self.runs.range(at..).next().map(|(s, _)| *s).unwrap_or(end);
                    let stop = next.min(end);
                    gaps.push((at, stop - at));
                    at = stop;
                }
            }
        }
        // No coalescing needed: a known run always separates two gaps, so
        // they can never abut.
        gaps
    }
}
