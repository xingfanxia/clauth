#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::providers::{StatRow, StatRowKind, ThirdPartyStats, UsageBar};
use crate::usage::UsageWindow;

fn window(util: f64, resets_at: Option<&str>) -> UsageWindow {
    UsageWindow {
        utilization: util,
        resets_at: resets_at.map(str::to_string),
    }
}

fn snapshot(name: &str, active: bool) -> ProfileSnapshot {
    ProfileSnapshot {
        name: name.to_string(),
        active,
        provider: "anthropic".to_string(),
        base_url: None,
        sub_type: Some("max".to_string()),
        five_h: Some(window(42.4, None)),
        seven_d: Some(window(7.6, None)),
        third_party: None,
    }
}

fn third_party_stats(
    bars: Vec<UsageBar>,
    rows: Vec<StatRow>,
    plan: Option<&str>,
) -> ThirdPartyStats {
    ThirdPartyStats {
        is_available: true,
        rows,
        bars,
        plan: plan.map(str::to_string),
        endpoint: None,
        best_effort: false,
    }
}

fn bar(label: &str, pct: f64) -> UsageBar {
    UsageBar {
        label: label.to_string(),
        pct,
        resets_at: None,
        used: None,
        total: None,
    }
}

fn row(label: &str, value: &str) -> StatRow {
    StatRow {
        label: label.to_string(),
        value: value.to_string(),
        kind: StatRowKind::Body,
    }
}

#[test]
fn usage_line_without_reset_is_label_and_pct() {
    assert_eq!(usage_line("5h", &window(42.4, None), 0), "5h 42% used");
}

#[test]
fn usage_line_rounds_and_shows_reset_countdown() {
    // now_secs one hour before the reset → "1h 0m".
    let reset = iso_to_epoch_secs("2026-06-21T12:00:00Z").unwrap();
    let line = usage_line(
        "7d",
        &window(99.6, Some("2026-06-21T12:00:00Z")),
        reset - 3600,
    );
    assert_eq!(line, "7d 100% used (resets in 1h 0m)");
}

#[test]
fn usage_line_falls_back_to_raw_on_unparseable_reset() {
    let line = usage_line("5h", &window(10.0, Some("not-a-date")), 0);
    assert_eq!(line, "5h 10% used (resets not-a-date)");
}

#[test]
fn third_party_headline_joins_bars_with_plan_prefix() {
    let s = third_party_stats(
        vec![bar("prompts", 50.0), bar("tokens", 12.4)],
        vec![],
        Some("pro"),
    );
    assert_eq!(third_party_headline(&s), "pro — prompts 50%, tokens 12%");
}

#[test]
fn third_party_headline_falls_back_to_first_row() {
    let s = third_party_stats(vec![], vec![row("balance", "$4.20")], None);
    assert_eq!(third_party_headline(&s), "balance: $4.20");
}

#[test]
fn third_party_headline_skips_value_less_heading_row() {
    // DeepSeek's first row is a value-less `USD balance` heading; the headline must
    // skip it and surface the first row that actually carries a value, never a
    // dangling `USD balance:` with nothing after it.
    let s = third_party_stats(
        vec![],
        vec![row("USD balance", ""), row("total", "$4.20")],
        None,
    );
    assert_eq!(third_party_headline(&s), "total: $4.20");
}

#[test]
fn third_party_headline_bare_plan_when_no_bars_or_rows() {
    // plan present, nothing else, still available → just the plan label.
    let s = third_party_stats(vec![], vec![], Some("pro"));
    assert_eq!(third_party_headline(&s), "pro");
}

#[test]
fn third_party_headline_unavailable_when_empty() {
    let mut s = third_party_stats(vec![], vec![], None);
    s.is_available = false;
    assert_eq!(third_party_headline(&s), "unavailable");
}

#[test]
fn live_footer_joins_present_parts() {
    let five = window(33.0, None);
    let seven = window(8.0, None);
    assert_eq!(
        live_footer(Some("work"), Some(&five), Some(&seven)),
        "active=work | 5h 33% used | 7d 8% used"
    );
}

#[test]
fn live_footer_omits_absent_parts() {
    assert_eq!(live_footer(None, None, None), "");
    assert_eq!(live_footer(Some("x"), None, None), "active=x");
}

#[test]
fn instructions_block_carries_staleness_nudge_and_profile_lines() {
    let profiles = vec![snapshot("work", true), snapshot("personal", false)];
    let out = instructions_block(&profiles, &SessionAuth::Global, "3m ago", 0);

    // staleness nudge: the % used legend, active-profile cache age, and the
    // "call list_profiles" pointer.
    assert!(out.contains("share of each window already used"));
    assert!(out.contains("active profile cached 3m ago"));
    assert!(out.contains("call `list_profiles` for live figures"));

    // at least one profile line, with the active marker and both usage windows.
    assert!(out.contains("- work (active) [anthropic, max]"));
    assert!(out.contains("5h 42% used"));
    assert!(out.contains("7d 8% used"));
    assert!(out.contains("- personal [anthropic, max]"));

    // load-bearing safety prose: dropping any of these warnings must fail here.
    assert!(
        out.contains("BURNS a real account usage window"),
        "the `delegate` quota-burn warning must survive a prose edit",
    );
    assert!(
        out.contains("hard-capped at depth 1"),
        "the delegation depth cap must survive a prose edit",
    );
    // the session-aware switch note must survive a prose edit (Global variant here).
    assert!(
        out.contains("switch & this session:"),
        "the `switch` effect note must survive a prose edit",
    );
    assert!(
        out.contains("its next token refresh"),
        "the global-session switch caveat must survive a prose edit",
    );
}

#[test]
fn switch_effect_distinguishes_global_from_isolated_sessions() {
    // Global: warns the current session's identity changes on next refresh.
    let global = switch_effect(&SessionAuth::Global);
    assert!(global.contains("THIS session reads"));
    assert!(global.contains("next token refresh"));
    assert!(global.contains("use the `delegate` tool"));

    // Isolated runtime: names the pinned profile and states it is unaffected.
    let pinned = switch_effect(&SessionAuth::IsolatedRuntime("work".to_string()));
    assert!(pinned.contains("pinned to `work`"));
    assert!(pinned.contains("unaffected"));

    // Custom config dir: also unaffected, no profile name.
    let custom = switch_effect(&SessionAuth::IsolatedCustom);
    assert!(custom.contains("custom `CLAUDE_CONFIG_DIR`"));
    assert!(custom.contains("unaffected"));
}

#[test]
fn instructions_block_uses_third_party_headline_for_provider_profiles() {
    let mut p = snapshot("deepseek", false);
    p.provider = "deepseek".to_string();
    p.third_party = Some("balance: $4.20".to_string());
    let out = instructions_block(&[p], &SessionAuth::Global, "1h 0m ago", 0);
    assert!(out.contains("- deepseek [deepseek, max]: balance: $4.20"));
}
