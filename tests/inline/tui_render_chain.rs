//! `member_detail`'s all-exhausted "resumes: <name> in ~<eta>" caption on the
//! Fallback tab (issue #10 follow-up), driven by `crate::fallback::soonest_resume`.

use super::*;
use crate::profile::{AppState, Profile, ProfileName};
use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
use std::collections::BTreeMap;

/// ISO reset `secs` in the future.
fn reset_in(secs: i64) -> String {
    epoch_secs_to_iso(now_epoch_secs() + secs)
}

fn profile(name: &str, threshold: f64, util: f64, reset_secs: i64) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: Some(threshold),
        last_resort: false,
        bell_threshold: None,
        credentials: None,
        usage: Some(UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: util,
                resets_at: Some(reset_in(reset_secs)),
            }),
            ..UsageInfo::default()
        }),
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

fn config_with(profiles: Vec<Profile>, active: Option<&str>, chain: Vec<&str>) -> AppConfig {
    let names: Vec<ProfileName> = profiles.iter().map(|p| p.name.clone()).collect();
    AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            profiles: names,
            fallback_chain: chain.into_iter().map(Into::into).collect(),
            ..AppState::default()
        },
        profiles,
    }
}

fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn resumes_line(lines: &[Line<'static>]) -> Option<String> {
    lines.iter().map(line_text).find(|t| t.contains("resumes:"))
}

// Whole chain exhausted: the caption renders under whichever member is
// selected, naming the soonest-resuming one (b resets sooner than a).
#[test]
fn all_exhausted_shows_resumes_hint_under_any_selected_member() {
    let a = profile("a", 95.0, 100.0, 3600);
    let b = profile("b", 95.0, 100.0, 1800);
    let cfg = config_with(vec![a, b], Some("a"), vec!["a", "b"]);

    let on_a = member_detail(&cfg, "a", 0, 2, false, 0, false, None, 60);
    let hint_a = resumes_line(&on_a).expect("resumes hint renders while viewing member a");
    assert!(hint_a.contains("resumes: b in ~"), "{hint_a}");

    let on_b = member_detail(&cfg, "b", 1, 2, false, 0, false, None, 60);
    let hint_b = resumes_line(&on_b).expect("resumes hint renders while viewing member b");
    assert!(hint_b.contains("resumes: b in ~"), "{hint_b}");
}

// b still has headroom — chain isn't fully exhausted, caption stays hidden.
#[test]
fn partially_exhausted_chain_hides_resumes_hint() {
    let a = profile("a", 95.0, 100.0, 3600);
    let b = profile("b", 95.0, 20.0, 3600);
    let cfg = config_with(vec![a, b], Some("a"), vec!["a", "b"]);

    let lines = member_detail(&cfg, "a", 0, 2, false, 0, false, None, 60);
    assert!(
        resumes_line(&lines).is_none(),
        "must not show when the chain isn't fully exhausted"
    );
}

// ── help-hint wrapping + dynamic copy ────────────────────────────────────────

// A narrow detail pane wraps the selected row's hint into `└ `-led +
// indented continuation lines instead of clipping it off the pane edge.
#[test]
fn last_resort_hint_wraps_on_a_narrow_pane() {
    let a = profile("a", 95.0, 20.0, 3600);
    let b = profile("b", 95.0, 20.0, 3600);
    let cfg = config_with(vec![a, b], Some("a"), vec!["a", "b"]);

    // Focused on the `last resort` row (FALLBACK_ROWS[1]) at 28 cols.
    let lines = member_detail(&cfg, "a", 0, 2, true, 1, false, None, 28);
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    let lead = texts
        .iter()
        .position(|t| t.starts_with("  └ "))
        .expect("hint leader line renders");
    assert!(
        texts[lead].chars().count() <= 28,
        "first hint line must fit the pane: {:?}",
        texts[lead]
    );
    assert!(
        texts[lead + 1].starts_with("    "),
        "hint continues on an indented line instead of clipping: {:?}",
        texts[lead + 1]
    );
}

// The last-resort hint names the member the exclusive mark would move from.
#[test]
fn last_resort_hint_names_the_currently_marked_member() {
    let a = profile("a", 95.0, 20.0, 3600);
    let mut b = profile("b", 95.0, 20.0, 3600);
    b.last_resort = true;
    let cfg = config_with(vec![a, b], Some("a"), vec!["a", "b"]);

    let lines = member_detail(&cfg, "a", 0, 2, true, 1, false, None, 80);
    let hint = lines
        .iter()
        .map(line_text)
        .find(|t| t.contains("└"))
        .expect("hint renders");
    assert!(hint.contains("from 'b'"), "{hint}");
}
