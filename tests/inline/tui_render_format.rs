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

// ── fetch-state cue ──────────────────────────────────────────────────────
//
// `fetch_cue_color`: amber = serving last-known numbers, red = failed, none =
// live. The overview countdown carries this cue; brackets stay plain dim.

fn cue_profile(status: Option<FetchStatus>) -> Profile {
    Profile {
        harness: Default::default(),
        name: "p".into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: std::collections::BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
        bell_threshold: None,
        credentials: None,
        usage: None,
        fetch_status: status,
        provider: None,
        third_party_usage: None,
    }
}

fn cue_window(util: f64) -> UsageWindow {
    UsageWindow {
        utilization: util,
        resets_at: None,
    }
}

#[test]
fn cue_amber_on_cached_and_rate_limited() {
    assert_eq!(
        fetch_cue_color(&cue_profile(Some(FetchStatus::Cached))),
        Some(theme::warning_color())
    );
    assert_eq!(
        fetch_cue_color(&cue_profile(Some(FetchStatus::RateLimited))),
        Some(theme::warning_color())
    );
}

#[test]
fn cue_red_on_failed() {
    assert_eq!(
        fetch_cue_color(&cue_profile(Some(FetchStatus::Failed))),
        Some(theme::danger_color())
    );
}

#[test]
fn cue_absent_when_live_or_never_fetched() {
    assert_eq!(
        fetch_cue_color(&cue_profile(Some(FetchStatus::Fresh))),
        None
    );
    assert_eq!(fetch_cue_color(&cue_profile(None)), None);
}

/// API-key/provider rows have no oauth fetch leg — a stray status must not
/// paint their brackets.
#[test]
fn cue_absent_for_api_key_profiles() {
    let mut p = cue_profile(Some(FetchStatus::Failed));
    p.base_url = Some("https://api.example.com".into());
    assert_eq!(fetch_cue_color(&p), None);
}

#[test]
fn brackets_stay_dim_regardless_of_fetch_state() {
    let w = cue_window(50.0);
    let spans = window_summary_spans_bracketed(Some(&w), 17, true, None);
    assert_eq!(spans[0].content, "[");
    assert_eq!(spans[0].style.fg, theme::dim().fg);
    assert_eq!(spans[2].content, "]");
    assert_eq!(spans[2].style.fg, theme::dim().fg);
}

/// A failed fetch with nothing cached renders only `—`, styled faint like any
/// other no-data cell — the cue lives on the overview countdown instead.
#[test]
fn no_data_dash_stays_faint() {
    let spans = window_summary_spans_bracketed(None, 17, true, None);
    assert_eq!(spans[0].content, "—");
    assert_eq!(spans[0].style.fg, theme::faint().fg);
}

// CDX-1 T8: the kind column is the harness tag for codex profiles.
#[test]
fn account_type_label_tags_codex_profiles() {
    let mut p = crate::testutil::blank_profile("cdx");
    p.harness = crate::profile::Harness::Codex;
    assert_eq!(account_type_label(&p), "Codex");
}
