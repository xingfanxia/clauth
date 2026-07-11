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
fn third_party_headline_joins_bars_with_plan_prefix() {
    let s = third_party_stats(
        vec![bar("prompts", 50.0), bar("tokens", 12.4)],
        vec![],
        Some("pro"),
    );
    assert_eq!(third_party_headline(&s), "pro: prompts 50%, tokens 12%");
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
fn instructions_block_emits_stable_roster_cost_model_and_safety_prose() {
    let profiles = vec![snapshot("work", true), snapshot("personal", false)];
    let out = instructions_block(&profiles, &SessionAuth::Global);

    // roster lines: identity only, with the active marker.
    assert!(out.contains("- work (active) [anthropic, max]"));
    assert!(out.contains("- personal [anthropic, max]"));

    // the roster is labelled a session-start snapshot with a live-refresh pointer.
    assert!(out.contains("Profiles (at session start"));
    assert!(out.contains("call `list_profiles`"));

    // cost model is spelled out so delegate routing can account for money.
    assert!(out.contains("Cost:"));
    assert!(out.contains("bills real USD"));

    // cheapest-target pointer + delegate-framing nudge must survive a prose edit.
    assert!(
        out.contains("call `list_profiles` for live windows"),
        "the cheapest-target routing pointer must survive a prose edit",
    );
    assert!(
        out.contains("A delegate sees nothing but the prompt"),
        "the delegate-framing nudge must survive a prose edit",
    );

    // volatile figures are NOT baked in — they rot within a turn, so they must
    // stay on the per-call `list_profiles` path, never here.
    assert!(
        !out.contains("% used"),
        "no usage percentages in the boot block"
    );

    // load-bearing safety prose: dropping any of these must fail here.
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
