use super::super::panes::wrap_words;

#[test]
fn short_line_unchanged() {
    assert_eq!(wrap_words("hello world", 36), vec!["hello world"]);
}

#[test]
fn wraps_at_word_boundary() {
    let out = wrap_words("terminal too small · enlarge for full layout", 36);
    assert_eq!(out.len(), 2, "expected 2 wrapped lines, got {out:?}");
    for l in &out {
        assert!(
            l.chars().count() <= 36,
            "line exceeds cap: {l:?} ({} chars)",
            l.chars().count()
        );
    }
}

#[test]
fn empty_input_yields_one_empty_line() {
    assert_eq!(wrap_words("", 36), vec![""]);
}

#[test]
fn single_word_exceeding_cap_hard_breaks() {
    let long_word = "a".repeat(80);
    let out = wrap_words(&long_word, 36);
    assert_eq!(out.len(), 3);
    for l in &out {
        assert!(l.chars().count() <= 36);
    }
}

#[test]
fn col_width_equals_content_width_plus_chrome() {
    let msg = "· enlarge for full layout";
    let content_cap: u16 = 36;
    let max_content_width = [msg]
        .iter()
        .map(|l| l.chars().count() as u16)
        .max()
        .unwrap()
        .min(content_cap);
    let col_width = max_content_width + 3;
    assert_eq!(max_content_width, 25);
    assert_eq!(col_width, 28);
}
