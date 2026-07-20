//! `member_detail`'s all-exhausted "resumes: <name> in ~<eta>" caption on the
//! Fallback tab (issue #10 follow-up), driven by `crate::fallback::soonest_resume`.

use super::*;
use crate::profile::{AppState, Profile, ProfileName};
use crate::usage::{UsageInfo, UsageWindow, epoch_secs_to_iso, now_epoch_secs};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
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
        max_auto_spend: None,
        bell_threshold: None,
        disabled: false,
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

    let on_a = member_detail(&cfg, "a", 0, 2, false, 0, false, None, None, 60, None);
    let hint_a = resumes_line(&on_a).expect("resumes hint renders while viewing member a");
    assert!(hint_a.contains("resumes: b in ~"), "{hint_a}");

    let on_b = member_detail(&cfg, "b", 1, 2, false, 0, false, None, None, 60, None);
    let hint_b = resumes_line(&on_b).expect("resumes hint renders while viewing member b");
    assert!(hint_b.contains("resumes: b in ~"), "{hint_b}");
}

// b still has headroom — chain isn't fully exhausted, caption stays hidden.
#[test]
fn partially_exhausted_chain_hides_resumes_hint() {
    let a = profile("a", 95.0, 100.0, 3600);
    let b = profile("b", 95.0, 20.0, 3600);
    let cfg = config_with(vec![a, b], Some("a"), vec!["a", "b"]);

    let lines = member_detail(&cfg, "a", 0, 2, false, 0, false, None, None, 60, None);
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
    let lines = member_detail(&cfg, "a", 0, 2, true, 1, false, None, None, 28, None);
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    let lead = texts
        .iter()
        .position(|t| t.starts_with(" └ "))
        .expect("hint leader line renders");
    assert!(
        texts[lead].chars().count() <= 28,
        "first hint line must fit the pane: {:?}",
        texts[lead]
    );
    // Exactly the leader's width, so the continuation stacks under the text
    // rather than under the `└` (or one cell past it).
    assert!(
        texts[lead + 1].starts_with("   ") && !texts[lead + 1].starts_with("    "),
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

    let lines = member_detail(&cfg, "a", 0, 2, true, 1, false, None, None, 80, None);
    let hint = lines
        .iter()
        .map(line_text)
        .find(|t| t.contains("└"))
        .expect("hint renders");
    assert!(hint.contains("instead of 'b'"), "{hint}");
}

// The `max spend` hint names whichever half of the opt-in is holding spending
// back, and shows the REAL armed room when both are set. Five distinct copies,
// one per spend state — `spend_room` fails closed on money, so an unknown spend
// never reads as a $0 figure.
#[test]
fn max_spend_hint_covers_every_spend_state() {
    use crate::usage::SpendInfo;
    let hint = |budget_on: bool, ceiling: Option<f64>, spend: Option<SpendInfo>| -> String {
        let mut a = profile("a", 95.0, 40.0, 7200);
        a.max_auto_spend = ceiling;
        a.usage.as_mut().unwrap().spend = spend;
        let mut cfg = config_with(vec![a], Some("a"), vec!["a"]);
        cfg.state.spend_budget_switching = budget_on;
        max_spend_hint(&cfg, "a", cfg.profiles[0].max_auto_spend.unwrap_or(0.0))
    };
    let billing = |enabled: bool, used: Option<f64>| SpendInfo {
        enabled,
        used,
        limit: Some(20.0),
        percent: None,
        currency: None,
    };

    // 1. chain toggle off
    assert!(
        hint(false, Some(10.0), Some(billing(true, Some(1.0))))
            .contains("turn on allow extra usage")
    );
    // 2. no ceiling
    assert!(hint(true, None, Some(billing(true, Some(1.0)))).contains("type a ceiling"));
    // 3. account not billing
    assert!(
        hint(true, Some(10.0), Some(billing(false, Some(1.0))))
            .contains("isn't set up for paid usage")
    );
    // 4. spend unknown → the ceiling statement, never an invented $0 room
    let unknown = hint(true, Some(10.0), Some(billing(true, None)));
    assert!(unknown.contains("spends at most $10.00"), "{unknown}");
    // 5. armed → the real room: 0.9 * min(20, 10) - 1 = $8.00
    let armed = hint(true, Some(10.0), Some(billing(true, Some(1.0))));
    assert!(armed.contains("$8.00 left to spend"), "{armed}");
}

// ── key-column alignment ────────────────────────────────────────────────────

/// Column a row's value opens at: the first non-space cell past the key text.
/// `str::find` is byte-based, so re-count chars for the head to stay glyph-
/// accurate past any multi-byte arrow (e.g. `❯`).
fn value_col(key: &str, rendered: &str) -> usize {
    let after = rendered.find(key).expect("row renders its key") + key.len();
    let head_chars = rendered[..after].chars().count();
    let gap = rendered[after..].chars().take_while(|c| *c == ' ').count();
    head_chars + gap
}

// `last resort` is exactly the old `KEY_W` (11) chars, so a
// `saturating_sub(len).max(1)` pad pushed its value a column right of every
// other interactive row. The shared `key_cell` keeps the gap separate from the
// width, so every interactive row opens its value at the same column.
#[test]
fn last_resort_value_aligns_with_other_rows() {
    let a = profile("a", 95.0, 20.0, 3600);
    let cfg = config_with(vec![a], Some("a"), vec!["a"]);
    let texts: Vec<String> = member_detail(&cfg, "a", 0, 1, true, 1, false, None, None, 60, None)
        .iter()
        .map(line_text)
        .collect();

    let rotate = texts
        .iter()
        .find(|t| t.contains("rotate at"))
        .expect("rotate at row");
    let last = texts
        .iter()
        .find(|t| t.contains("last resort"))
        .expect("last resort row");
    let remove = texts
        .iter()
        .find(|t| t.contains("remove"))
        .expect("remove row");

    let col = value_col("rotate at", rotate);
    assert_eq!(
        col,
        value_col("last resort", last),
        "`last resort` (== old KEY_W chars) must not push its value column right"
    );
    assert_eq!(
        col,
        value_col("remove", remove),
        "all rows share the value column"
    );

    let spend = texts
        .iter()
        .find(|t| t.contains("max spend"))
        .expect("max spend row");
    assert_eq!(
        col,
        value_col("max spend", spend),
        "all rows share the value column"
    );
}

/// The ceiling row reads as a state, not a bare number: $0 is the never-spend
/// default and must not look like a figure the operator dialled in. A set
/// ceiling renders as money, with the cents, since it is money.
#[test]
fn max_spend_row_renders_off_at_zero_and_dollars_when_set() {
    let cfg = config_with(vec![profile("a", 95.0, 20.0, 3600)], Some("a"), vec!["a"]);
    let row = |c: &crate::profile::AppConfig| -> String {
        member_detail(c, "a", 0, 1, true, 1, false, None, None, 60, None)
            .iter()
            .map(line_text)
            .find(|t| t.contains("max spend"))
            .expect("max spend row")
    };
    assert!(
        row(&cfg).contains("off"),
        "unset reads as off: {:?}",
        row(&cfg)
    );
    assert!(!row(&cfg).contains('$'), "no dollar figure when off");

    let mut armed = config_with(vec![profile("a", 95.0, 20.0, 3600)], Some("a"), vec!["a"]);
    armed.profiles[0].max_auto_spend = Some(25.0);
    assert!(
        row(&armed).contains("$25.00"),
        "a set ceiling renders as money: {:?}",
        row(&armed)
    );
}

// ── disabled chain member (feature: per-account disable toggle) ─────────────

/// A disabled chain member — still configured in `fallback_chain` on disk,
/// only the walk skips it (see `Profile::is_disabled` / `docs/internals.md`)
/// — renders dimmed with a `[ disabled ]` chip in the Fallback tab's own
/// selector list, in place of the usual blocked-reason marker. The add-picker
/// exclusion (a disabled account can never be (re-)added) is a pure-logic
/// concern covered separately in `tests/inline/tui_app.rs`'s
/// `chain_candidates_excludes_a_disabled_profile`.
#[test]
fn disabled_chain_member_dims_and_shows_chip_in_the_selector() {
    let mut a = profile("xqzacct", 95.0, 10.0, 3600);
    a.disabled = true;
    let cfg = config_with(vec![a], None, vec!["xqzacct"]);
    let app = App::new(cfg);

    let (w, h) = (100u16, 14u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    let buf = term.backend().buffer();
    let rows = crate::testutil::buffer_rows(buf);

    // The detail pane's own title border also carries the bare name
    // (`section_box_verbatim`), so require the chip alongside it to land on
    // the selector's list row specifically.
    let row_idx = rows
        .iter()
        .position(|r| r.contains("xqzacct") && r.contains("disabled"))
        .unwrap_or_else(|| {
            panic!(
                "no row carries both the member name and the chip:\n{}",
                rows.join("\n")
            )
        });
    let row = &rows[row_idx];
    let name_col = row.find("xqzacct").expect("name renders");
    let cell = &buf.content[row_idx * w as usize + name_col];
    assert_eq!(
        Some(cell.fg),
        theme::dim().fg,
        "the disabled member's name cell renders dim, not name_color's active/inactive branch"
    );

    // The pill LABEL (not its brackets) is bold — the cloudy-tui neutral-pill
    // rule (TEXT_DIM + bold), matching `reason_pill`'s `Stale` arm.
    let label_col = row.find("disabled").expect("chip label renders");
    let label_cell = &buf.content[row_idx * w as usize + label_col];
    assert!(
        label_cell.modifier.contains(ratatui::style::Modifier::BOLD),
        "the disabled pill's label must be bold"
    );
}

// ── blocked-reason pill (detail card, weekly-fallback §4) ────────────────────

// A member over its 5h threshold shows the worst-reason pill at the very top of
// the card, naming the block with its utilization % and reset countdown.
#[test]
fn blocked_member_shows_the_worst_reason_pill() {
    let cfg = config_with(vec![profile("a", 95.0, 97.0, 7200)], Some("a"), vec!["a"]);
    let lines = member_detail(&cfg, "a", 0, 1, false, 0, false, None, None, 60, None);
    let pill = line_text(&lines[0]);
    assert!(pill.contains('['), "renders as a status pill: {pill:?}");
    assert!(
        pill.contains("5h 97%"),
        "names the 5h block with %: {pill:?}"
    );
    // Tolerant on the exact bucket: the fixture's `now` and `blocked_reason`'s
    // `now` can straddle a whole second (7200 → "1h 59m"), so assert only that a
    // countdown suffix renders, not its value.
    assert!(
        pill.contains("5h 97% · "),
        "carries the reset countdown: {pill:?}"
    );
}

// A switch-grade kick-rejected member — headroom, but the messages limiter won't
// let clauth start it — shows the `claude code blocked` pill driven by `kick_lift`.
#[test]
fn kick_rejected_member_shows_the_claude_code_blocked_pill() {
    let cfg = config_with(vec![profile("a", 95.0, 40.0, 7200)], Some("a"), vec!["a"]);
    let until = now_epoch_secs() + 7200;
    let lines = member_detail(
        &cfg,
        "a",
        0,
        1,
        false,
        0,
        false,
        None,
        None,
        60,
        Some(until),
    );
    let pill = line_text(&lines[0]);
    // Label + a countdown suffix; the exact bucket stays tolerant since the two
    // `now` reads (fixture vs `blocked_reason`) can straddle a whole second. The
    // exact `lifts_in` value is range-checked in the `blocked_reason_kick_*` unit
    // test instead.
    assert!(
        pill.contains("claude code blocked · "),
        "renders the kick pill with a lift countdown: {pill:?}"
    );
}

// A member with headroom shows no pill — the card opens straight on `priority`.
#[test]
fn headroom_member_shows_no_reason_pill() {
    let cfg = config_with(vec![profile("a", 95.0, 40.0, 7200)], Some("a"), vec!["a"]);
    let lines = member_detail(&cfg, "a", 0, 1, false, 0, false, None, None, 60, None);
    let first = line_text(&lines[0]);
    assert!(
        !first.contains('['),
        "no pill for a member with headroom: {first:?}"
    );
    assert!(
        first.contains("priority"),
        "card opens on priority: {first:?}"
    );
}

// The pill occupies exactly `PILL_LINES` rows (pill + blank) above `priority` —
// the count `draw_chain_detail` folds into the native-cursor row math.
#[test]
fn blocked_pill_occupies_pill_lines_above_priority() {
    let cfg = config_with(vec![profile("a", 95.0, 100.0, 7200)], Some("a"), vec!["a"]);
    let lines = member_detail(&cfg, "a", 0, 1, false, 0, false, None, None, 60, None);
    let priority_at = lines
        .iter()
        .position(|l| line_text(l).contains("priority"))
        .expect("priority row renders");
    assert_eq!(priority_at, PILL_LINES, "pill + blank precede priority");
}

// ── `max spend` dims while inert (spend budget off) ──────────────────────────

fn span_style(line: &Line<'static>, needle: &str) -> Option<ratatui::style::Style> {
    line.spans
        .iter()
        .find(|s| s.content.contains(needle))
        .map(|s| s.style)
}

// A set ceiling with the chain-wide `spend budget` OFF spends nothing, so it must
// not read as armed: render the value faint. Flip spend budget on and the same
// ceiling renders in ACCENT.
#[test]
fn max_spend_dims_when_spend_budget_is_off() {
    let mut cfg = config_with(vec![profile("a", 95.0, 40.0, 3600)], Some("a"), vec!["a"]);
    cfg.profiles[0].max_auto_spend = Some(25.0);

    let off = member_detail(&cfg, "a", 0, 1, true, 2, false, None, None, 60, None);
    let off_val = off
        .iter()
        .find_map(|l| span_style(l, "$25.00"))
        .expect("max spend ceiling renders");
    assert_eq!(
        off_val.fg,
        crate::tui::theme::faint().fg,
        "an inert ceiling (spend budget off) renders faint"
    );

    cfg.state.spend_budget_switching = true;
    let on = member_detail(&cfg, "a", 0, 1, true, 2, false, None, None, 60, None);
    let on_val = on
        .iter()
        .find_map(|l| span_style(l, "$25.00"))
        .expect("max spend ceiling renders");
    assert_eq!(
        on_val.fg,
        crate::tui::theme::accent().fg,
        "an armed ceiling (spend budget on) renders in accent"
    );
}
