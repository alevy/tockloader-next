//! Merging byte ranges, so one logical operation does not become many small
//! requests to the device.

/// Sort and merge `(address, len)` ranges, joining any pair separated by `gap`
/// bytes or fewer. Empty ranges are dropped.
pub fn coalesce(ranges: &[(u64, u64)], gap: u64) -> Vec<(u64, u64)> {
    let mut sorted: Vec<(u64, u64)> = ranges.iter().copied().filter(|(_, l)| *l > 0).collect();
    sorted.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(sorted.len());
    for (address, len) in sorted {
        match out.last_mut() {
            Some((start, open)) if address <= *start + *open + gap => {
                *open = (*open).max(address + len - *start);
            }
            _ => out.push((address, len)),
        }
    }
    out
}

#[cfg(test)]
mod tests;
