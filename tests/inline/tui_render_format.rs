//! `fixed_split` truncation contract: content + pad always total `width`, and
//! ANY dropped character is signalled with a trailing `…` — including the
//! boundary case where the value is exactly one char over the window, which
//! the old `for`-loop consumed and mistook for end-of-string.

use super::*;

fn joined(value: &str, width: usize) -> String {
    let (content, pad) = fixed_split(value, width);
    format!("{content}{pad}")
}

#[test]
fn fits_exactly_no_ellipsis() {
    assert_eq!(fixed_split("Max 20", 6), ("Max 20".into(), "".into()));
}

#[test]
fn shorter_pads_to_width() {
    assert_eq!(fixed_split("ok", 5), ("ok".into(), "   ".into()));
}

/// The off-by-one: one char over the window must still truncate visibly.
#[test]
fn one_char_over_truncates_with_ellipsis() {
    assert_eq!(fixed_split("Max 20x", 6).0, "Max 2…");
    assert_eq!(fixed_split("x@computelabs.ai", 15).0, "x@computelabs.…");
}

#[test]
fn far_over_truncates_with_ellipsis() {
    assert_eq!(fixed_split("a-long-account-name", 8).0, "a-long-…");
}

#[test]
fn width_zero_yields_nothing() {
    assert_eq!(fixed_split("anything", 0), (String::new(), String::new()));
}

/// Invariant across the boundary: rendered cell is always exactly `width`
/// chars for any non-empty value.
#[test]
fn cell_is_always_exactly_width() {
    for len in 0..12usize {
        let value: String = "abcdefghijkl".chars().take(len).collect();
        for width in 1..10usize {
            assert_eq!(
                joined(&value, width).chars().count(),
                width,
                "value len {len}, width {width}"
            );
        }
    }
}
