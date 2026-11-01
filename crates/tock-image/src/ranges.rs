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
mod tests {
    use super::*;

    #[test]
    fn merges_adjacent_and_nearby_ranges() {
        // Touching, and within the gap, and far apart.
        assert_eq!(coalesce(&[(0, 8), (8, 8)], 0), vec![(0, 16)]);
        assert_eq!(coalesce(&[(0, 8), (12, 4)], 4), vec![(0, 16)]);
        assert_eq!(coalesce(&[(0, 8), (12, 4)], 3), vec![(0, 8), (12, 4)]);
    }

    #[test]
    fn sorts_and_absorbs_overlaps() {
        assert_eq!(coalesce(&[(100, 10), (0, 8)], 0), vec![(0, 8), (100, 10)]);
        assert_eq!(coalesce(&[(0, 16), (4, 4)], 0), vec![(0, 16)]);
        assert_eq!(coalesce(&[(0, 16), (8, 16)], 0), vec![(0, 24)]);
    }

    #[test]
    fn drops_empty_ranges() {
        assert_eq!(coalesce(&[(0, 0)], 0), vec![]);
        assert_eq!(coalesce(&[], 0), vec![]);
        assert_eq!(coalesce(&[(0, 0), (8, 4)], 0), vec![(8, 4)]);
    }
}
