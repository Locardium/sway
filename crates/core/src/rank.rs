//! Fractional ranks (LexoRank-style) for the manual ordering of playlists,
//! folders, and tracks within a playlist.
//!
//! Why not a sequential `position INTEGER`: moving an element renumbers all
//! of its siblings, so two devices reordering the same playlist offline
//! produce writes that clobber each other — the only possible resolution
//! would be to keep one order and throw away the other entirely.
//!
//! With a fractional rank, inserting between two neighbors generates an
//! intermediate string and **touches no other row**. Two concurrent
//! reorderings touch different rows and merge without conflict.
//!
//! The alphabet is 62 ASCII characters in ascending order, so comparing
//! ranks as text (BINARY collation, SQLite's default) gives the same result
//! as comparing them digit by digit. `ORDER BY rank` is enough.

const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BASE: i32 = 62;

fn digit(c: u8) -> i32 {
    ALPHABET.iter().position(|&a| a == c).unwrap_or(0) as i32
}

/// Generates a rank strictly between `prev` and `next`.
/// `None` means "no neighbor on that side" (start or end of the list).
///
/// Never returns a string ending in the minimum digit: a new digit is only
/// emitted when there's real room between the two neighbors, so there's
/// always space to insert again on either side.
pub fn between(prev: Option<&str>, next: Option<&str>) -> String {
    // Inverted order (shouldn't happen): degrade to "after prev" instead of
    // generating an invalid rank that silently breaks the order.
    if let (Some(p), Some(n)) = (prev, next) {
        if p >= n {
            return between(Some(p), None);
        }
    }
    let p = prev.unwrap_or("").as_bytes();
    let n = next.unwrap_or("").as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let pd = if i < p.len() { digit(p[i]) } else { 0 };
        // Past the end of `next` the gap reaches the top of the alphabet;
        // same if there's no `next`.
        let nd = if i < n.len() { digit(n[i]) } else { BASE };
        if nd - pd > 1 {
            out.push(ALPHABET[((pd + nd) / 2) as usize]);
            break;
        }
        // No room at this digit: copy `prev`'s and refine in the next one.
        // The string only grows as much as needed.
        out.push(if i < p.len() { p[i] } else { ALPHABET[0] });
        i += 1;
    }
    String::from_utf8(out).expect("ASCII alphabet")
}

/// Ranks for a list numbered from zero (initial import, migration).
/// Spaced to leave room in between without having to lengthen strings.
pub fn initial_ranks(count: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(count);
    let mut prev: Option<String> = None;
    for _ in 0..count {
        let r = between(prev.as_deref(), None);
        prev = Some(r.clone());
        out.push(r);
    }
    out
}

/// Rank to insert at `index` within `siblings` (already ordered).
pub fn rank_at(siblings: &[String], index: usize) -> String {
    let index = index.min(siblings.len());
    let prev = if index == 0 { None } else { siblings.get(index - 1).map(|s| s.as_str()) };
    let next = siblings.get(index).map(|s| s.as_str());
    between(prev, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn between_is_strictly_ordered() {
        let a = between(None, None);
        let before = between(None, Some(&a));
        let after = between(Some(&a), None);
        assert!(before < a, "{before} < {a}");
        assert!(a < after, "{a} < {after}");
        let mid = between(Some(&before), Some(&a));
        assert!(before < mid && mid < a, "{before} < {mid} < {a}");
    }

    #[test]
    fn repeated_insertion_between_two_neighbours_keeps_order() {
        // The worst case: always inserting into the same gap. The strings
        // grow longer, but the order never breaks.
        let mut lo = between(None, None);
        let hi = between(Some(&lo), None);
        for _ in 0..200 {
            let mid = between(Some(&lo), Some(&hi));
            assert!(lo < mid && mid < hi, "{lo} < {mid} < {hi}");
            lo = mid;
        }
    }

    #[test]
    fn initial_ranks_are_ascending_and_unique() {
        let ranks = initial_ranks(50);
        for w in ranks.windows(2) {
            assert!(w[0] < w[1], "{} < {}", w[0], w[1]);
        }
        let mut sorted = ranks.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 50);
    }

    #[test]
    fn rank_at_places_element_at_index() {
        let siblings = initial_ranks(5);
        let first = rank_at(&siblings, 0);
        assert!(first < siblings[0]);
        let last = rank_at(&siblings, 5);
        assert!(last > siblings[4]);
        let middle = rank_at(&siblings, 2);
        assert!(siblings[1] < middle && middle < siblings[2]);
    }

    #[test]
    fn never_ends_in_min_digit() {
        // A rank ending in '0' leaves no room to insert right before it
        // without lengthening indefinitely; the generator must not produce them.
        let mut prev = between(None, None);
        for _ in 0..100 {
            assert!(!prev.ends_with('0'), "rank ends in 0: {prev}");
            prev = between(None, Some(&prev));
        }
    }

    #[test]
    fn inverted_input_degrades_to_after_prev() {
        let r = between(Some("z"), Some("a"));
        assert!(r > "z".to_string());
    }
}
