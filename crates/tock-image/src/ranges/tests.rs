//! Tests for [`coalesce`].

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
