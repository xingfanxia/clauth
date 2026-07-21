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

    let on_a = member_detail(&cfg, "a", false, 0, false, None, None, 60, None).0;
    let hint_a = resumes_line(&on_a).expect("resumes hint renders while viewing member a");
    assert!(hint_a.contains("resumes: b in ~"), "{hint_a}");

    let on_b = member_detail(&cfg, "b", false, 0, false, None, None, 60, None).0;
    let hint_b = resumes_line(&on_b).expect("resumes hint renders while viewing member b");
    assert!(hint_b.contains("resumes: b in ~"), "{hint_b}");
}

// b still has headroom — chain isn't fully exhausted, caption stays hidden.
#[test]
fn partially_exhausted_chain_hides_resumes_hint() {
    let a = profile("a", 95.0, 100.0, 3600);
    let b = profile("b", 95.0, 20.0, 3600);
    let cfg = config_with(vec![a, b], Some("a"), vec!["a", "b"]);

    let lines = member_detail(&cfg, "a", false, 0, false, None, None, 60, None).0;
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
    let lines = member_detail(&cfg, "a", true, 1, false, None, None, 28, None).0;
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

    let lines = member_detail(&cfg, "a", true, 1, false, None, None, 80, None).0;
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
    let texts: Vec<String> = member_detail(&cfg, "a", true, 1, false, None, None, 60, None)
        .0
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
        member_detail(c, "a", true, 1, false, None, None, 60, None)
            .0
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

/// `Disabled` and `Canceled` share the `⊖` shape and split on hue alone — the
/// one deliberate departure from cloudy-tui's shape-names-the-state rule (see
/// `reason_marker`). Pinned here because giving either arm its own shape puts
/// the same account under two glyphs across the Overview's two panels: the
/// account row picks the canceled arm where this ladder picks the disabled one.
#[test]
fn disabled_and_canceled_share_the_marker_shape_and_split_on_hue() {
    let dis = reason_marker(&BlockedReason::Disabled);
    let can = reason_marker(&BlockedReason::Canceled);
    assert_eq!(dis.content, "⊖", "disabled marker shape");
    assert_eq!(can.content, "⊖", "canceled marker shape");
    assert_eq!(dis.style.fg, theme::faint().fg, "disabled reads uncharged");
    assert_eq!(can.style.fg, theme::danger().fg, "canceled reads dead");
}

/// A disabled chain member — still configured in `fallback_chain` on disk,
/// only the walk skips it (see `Profile::is_disabled` / `docs/internals.md`)
/// — dims its name in the Fallback selector and carries the `⊖` blocked-reason
/// marker, with the `[ disabled ]` label reaching the operator through the
/// detail card's `reason_pill`. The add-picker exclusion (a disabled account
/// can never be (re-)added) is a pure-logic concern covered separately in
/// `tests/inline/tui_app.rs`'s `chain_candidates_excludes_a_disabled_profile`.
#[test]
fn disabled_chain_member_dims_its_name_and_takes_the_blocked_reason_marker() {
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
    // (`section_box_verbatim`), so require the marker alongside it to land on
    // the selector's list row specifically.
    let row_idx = rows
        .iter()
        .position(|r| r.contains("xqzacct") && r.contains('⊖'))
        .unwrap_or_else(|| {
            panic!(
                "no row carries both the member name and the ⊖ marker:\n{}",
                rows.join("\n")
            )
        });
    let row = &rows[row_idx];
    // Buffer COLUMN, not `str::find`'s byte offset — the pane border and the
    // marker are multi-byte, so the two diverge on exactly this row.
    let col_of = |needle: &str| -> usize {
        let byte = row
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} renders"));
        row[..byte].chars().count()
    };
    let cell = &buf.content[row_idx * w as usize + col_of("xqzacct")];
    assert_eq!(
        Some(cell.fg),
        theme::dim().fg,
        "the disabled member's name cell renders dim, not name_color's active/inactive branch"
    );

    let marker_cell = &buf.content[row_idx * w as usize + col_of("⊖")];
    assert_eq!(
        Some(marker_cell.fg),
        theme::faint().fg,
        "the ⊖ marker is uncharged, matching ⋯ stale"
    );
    // ⊖ is shared with `Canceled` (see `reason_marker`), so the hue is the only
    // thing telling the two apart on this row.
    assert_ne!(
        Some(marker_cell.fg),
        theme::danger().fg,
        "a disabled member must not wear the canceled arm's danger hue"
    );

    // Both panes share this physical row, so split at the seam: the selector
    // half carries the marker alone, the label lives on the detail card's pill.
    let seam = row.find("││").expect("the two panes meet on this row");
    let (selector, detail) = row.split_at(seam);
    assert!(
        !selector.contains("disabled"),
        "the selector row carries the marker only, no inline chip: {selector}"
    );
    assert!(
        detail.contains("[ disabled ]"),
        "the detail card shows the `[ disabled ]` pill: {detail}"
    );
}

/// `BlockedReason::Disabled` outranks every other reason: a disabled account is
/// skipped as a candidate regardless of what its usage or credentials say, so
/// naming a quota/liveness block instead would describe a member nothing picks.
#[test]
fn blocked_reason_ranks_disabled_above_canceled_and_auth_broken() {
    use crate::fallback::{BlockedReason, blocked_reason};
    use crate::usage::PlanInfo;

    // Canceled subscription AND a broken login AND a maxed 5h window at once.
    let mut a = profile("acct", 50.0, 100.0, 3600);
    a.disabled = true;
    a.usage.as_mut().unwrap().plan = Some(PlanInfo {
        subscription_status: Some("canceled".to_string()),
        ..PlanInfo::default()
    });
    let mut cfg = config_with(vec![a], Some("other"), vec!["acct"]);
    cfg.state.auth_broken.push("acct".into());

    let p = cfg.find("acct").unwrap();
    assert_eq!(
        blocked_reason(&cfg, p, None),
        Some(BlockedReason::Disabled),
        "disabled ranks first, above canceled and auth broken"
    );

    // Flipping only the disabled bit hands the row back to the next rung.
    let mut enabled = cfg.clone();
    enabled.profiles[0].disabled = false;
    assert_eq!(
        blocked_reason(&enabled, enabled.find("acct").unwrap(), None),
        Some(BlockedReason::Canceled),
        "without the disabled bit the canceled rung wins"
    );
}

/// The non-active guard: `snapshot_chain` / `next_target` deliberately never
/// drop a disabled ACTIVE member from the walk (the bit is candidate-only), so
/// claiming `Disabled` there would be the second opinion `blocked_reason` must
/// never be. Unreachable through `actions::disable_profile` today — which
/// refuses an active target — but the walk already guards it, so this does too.
#[test]
fn blocked_reason_never_reports_disabled_for_the_active_profile() {
    use crate::fallback::{BlockedReason, blocked_reason};

    let mut a = profile("acct", 95.0, 10.0, 3600);
    a.disabled = true;
    let cfg = config_with(vec![a], Some("acct"), vec!["acct"]);
    assert_eq!(
        blocked_reason(&cfg, cfg.find("acct").unwrap(), None),
        None,
        "a disabled ACTIVE member has headroom and reports no block"
    );

    // The same profile, no longer active, does report it.
    let mut inactive = cfg.clone();
    inactive.state.active_profile = Some("other".into());
    assert_eq!(
        blocked_reason(&inactive, inactive.find("acct").unwrap(), None),
        Some(BlockedReason::Disabled),
        "a disabled NON-active member reports the block"
    );
}

// ── blocked-reason pill (detail card, weekly-fallback §4) ────────────────────

// A member over its 5h threshold shows the worst-reason pill at the very top of
// the card, naming the block with its utilization % and reset countdown.
#[test]
fn blocked_member_shows_the_worst_reason_pill() {
    let cfg = config_with(vec![profile("a", 95.0, 97.0, 7200)], Some("a"), vec!["a"]);
    let lines = member_detail(&cfg, "a", false, 0, false, None, None, 60, None).0;
    let pill = line_text(&lines[0]);
    assert!(pill.contains('['), "renders as a status pill: {pill:?}");
    assert!(
        pill.contains("5h 97%"),
        "names the 5h block with %: {pill:?}"
    );
    // Tolerant on the exact bucket: the fixture's `now` and `blocked_reason`'s
    // `now` can straddle a whole second (7200 → "1h 59m"), so assert only that a
    // countdown suffix trails the pill, not its value.
    assert!(
        pill.contains("[ 5h 97% ]  "),
        "carries the reset countdown as a trailing suffix: {pill:?}"
    );
    assert!(!pill.contains('·'), "no middle-dot separator: {pill:?}");
}

// A switch-grade kick-rejected member — headroom, but the messages limiter won't
// let clauth start it — shows the `claude code blocked` pill driven by `kick_lift`.
#[test]
fn kick_rejected_member_shows_the_claude_code_blocked_pill() {
    let cfg = config_with(vec![profile("a", 95.0, 40.0, 7200)], Some("a"), vec!["a"]);
    let until = now_epoch_secs() + 7200;
    let lines = member_detail(&cfg, "a", false, 0, false, None, None, 60, Some(until)).0;
    let pill = line_text(&lines[0]);
    // Bare pill + a faint countdown suffix OUTSIDE the brackets (no `·`), the
    // same shape the Usage-tab kick pill renders. The exact bucket stays tolerant
    // since the two `now` reads (fixture vs `blocked_reason`) can straddle a whole
    // second. The exact `lifts_in` value is range-checked in the
    // `blocked_reason_kick_*` unit test instead.
    assert!(
        pill.contains("[ claude code blocked ]  "),
        "renders the kick pill with a trailing lift countdown: {pill:?}"
    );
    assert!(!pill.contains('·'), "no middle-dot separator: {pill:?}");
}

// The canceled pill reads the short shared label, not the old verbose
// `subscription canceled`: the `└` hint carries the explanation, and the label
// comes from the one source both this card and the Usage status block read, so
// the two tabs can't drift apart again.
#[test]
fn canceled_member_shows_the_short_shared_label() {
    let rendered: String = reason_pill_spans(&BlockedReason::Canceled, ResetFmt::default())
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(rendered, "[ canceled ]", "got {rendered:?}");
    assert!(
        !rendered.contains("subscription"),
        "the verbose wording moved to the hint line: {rendered:?}"
    );
}

// A member with headroom shows no pill — the card opens straight on `5h usage`
// (chain position moved to the selector's `#n` rail, so the card no longer
// restates it).
#[test]
fn headroom_member_shows_no_reason_pill() {
    let cfg = config_with(vec![profile("a", 95.0, 40.0, 7200)], Some("a"), vec!["a"]);
    let lines = member_detail(&cfg, "a", false, 0, false, None, None, 60, None).0;
    let first = line_text(&lines[0]);
    assert!(
        !first.contains('['),
        "no pill for a member with headroom: {first:?}"
    );
    assert!(
        first.contains("5h usage"),
        "card opens on the 5h gauge: {first:?}"
    );
}

/// The `rows_start` `member_detail` RETURNS must be the index of the FIRST
/// `FALLBACK_ROWS` row it actually pushed, at every header height — 0 pills,
/// 1 pill + its fix line, and the stacked 2. That figure is what
/// `draw_chain_detail` adds to the native-cursor row math, so a drift puts a
/// typed field's caret on the wrong row, which no text-only assertion catches.
/// `rotate at` is the first `FALLBACK_ROWS` entry, so pinning
/// `rows_start == position_of("rotate at")` is the whole contract in one
/// equality — and it stays honest through a header-block change, unlike the
/// hand-maintained `ROWS_BEFORE` it replaced.
#[test]
fn member_detail_rows_start_indexes_the_first_fallback_row_at_every_header_height() {
    let at_width = |cfg: &AppConfig, width: usize| -> (usize, usize) {
        let (lines, rows_start) = member_detail(cfg, "a", false, 0, false, None, None, width, None);
        let first_row_at = lines
            .iter()
            .position(|l| line_text(l).contains("rotate at"))
            .expect("the first FALLBACK_ROWS row renders");
        (rows_start, first_row_at)
    };
    let start_and_first_row = |cfg: &AppConfig| at_width(cfg, 60);

    // 0 pills: gauge + headroom + blank only.
    let healthy = config_with(vec![profile("a", 95.0, 10.0, 7200)], Some("a"), vec!["a"]);
    let (start, row) = start_and_first_row(&healthy);
    assert_eq!(start, row, "no pill: rows_start indexes the first row");
    assert_eq!(start, 3, "gauge + headroom + blank");

    // 1 pill: adds the pill row, its `└` fix line, and the separating blank.
    let one = config_with(vec![profile("a", 95.0, 100.0, 7200)], Some("a"), vec!["a"]);
    let (start, row) = start_and_first_row(&one);
    assert_eq!(start, row, "1 pill: rows_start indexes the first row");
    assert_eq!(start, 6, "pill + hint + blank on top of the 3 base rows");

    // 2 pills: disabled AND auth-broken stack, each with its own fix line.
    let mut d = profile("a", 95.0, 10.0, 7200);
    d.disabled = true;
    let mut two = config_with(vec![d], Some("other"), vec!["a"]);
    two.state.auth_broken.push("a".into());
    let (start, row) = start_and_first_row(&two);
    assert_eq!(start, row, "2 pills: rows_start indexes the first row");
    assert_eq!(
        start, 8,
        "two pill+hint pairs + blank on top of the 3 base rows"
    );

    // Narrow: the SAME config must produce a taller header, because each fix
    // line now wraps. This is what makes `rows_start` load-bearing rather than a
    // function of the pill count — and it is what stops the caret test's
    // "wrapped" cases from being duplicates of its wide ones.
    let (narrow_start, narrow_row) = at_width(&two, 30);
    assert_eq!(
        narrow_start, narrow_row,
        "wrapped: rows_start still indexes the first row"
    );
    assert!(
        narrow_start > start,
        "a 30-col pane must wrap the fix lines and push the rows down \
         (wide={start}, narrow={narrow_start}) — otherwise nothing here tests wrapping"
    );
}

/// The caret math end-to-end, through the real `draw` path and the real
/// `frame.set_cursor_position` — the caret must land on the buffer row that
/// actually carries the `rotate at` field it is editing. Asserting against the
/// RENDERED row rather than an arithmetic delta is what makes this immune to
/// the header block changing height: it restates the user-visible contract
/// ("the caret is in the field") instead of re-deriving the implementation's
/// own sum. Driven at 0, 1 and 2 pills, and with the `priority` row gone.
///
/// The narrow cases are the whole reason `rows_start` exists: below ~40 columns
/// each fix line WRAPS, so the header block's height stops being a function of
/// the pill count alone. Kept tall (40 rows) so the row is always on-pane —
/// clipping is a separate contract, pinned by
/// `typed_threshold_caret_is_not_set_when_the_row_is_clipped_off_the_pane`.
#[test]
fn typed_threshold_caret_lands_on_the_rotate_at_row_at_every_header_height() {
    let check = |cfg: AppConfig, label: &str, w: u16, h: u16| {
        let mut app = App::new(cfg);
        app.fallback_focus = FallbackFocus::Detail;
        app.fallback_detail_cursor = FALLBACK_ROWS
            .iter()
            .position(|r| *r == FallbackRow::Threshold)
            .unwrap();
        app.fallback_threshold_draft = Some(InputState::new("80"));
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
        let caret = term
            .get_cursor_position()
            .expect("a typed field places the native caret");
        let rows = crate::testutil::buffer_rows(term.backend().buffer());
        let rendered_at = rows
            .iter()
            .position(|r| r.contains("rotate at"))
            .unwrap_or_else(|| panic!("[{label}] rotate at renders:\n{}", rows.join("\n")));
        assert_eq!(
            caret.y as usize,
            rendered_at,
            "[{label}] caret must sit on the rotate-at row, not {} rows off",
            (caret.y as i64) - (rendered_at as i64)
        );
    };

    let healthy = || config_with(vec![profile("a", 95.0, 10.0, 7200)], Some("a"), vec!["a"]);
    let one_pill = || config_with(vec![profile("a", 95.0, 100.0, 7200)], Some("a"), vec!["a"]);
    // 2 pills: disabled AND auth-broken, each with its own fix line.
    let two_pills = || {
        let mut d = profile("a", 95.0, 10.0, 7200);
        d.disabled = true;
        let mut two = config_with(vec![d], Some("other"), vec!["a"]);
        two.state.auth_broken.push("a".into());
        two
    };

    check(healthy(), "0 pills", 120, 30);
    check(one_pill(), "1 pill", 120, 30);
    check(two_pills(), "2 pills", 120, 30);

    // Wrapped hints: one fix line becomes 2-3 rows, so the header height is no
    // longer derivable from the pill count.
    check(one_pill(), "1 pill, wrapped", 34, 40);
    check(two_pills(), "2 pills, wrapped", 34, 40);
    check(two_pills(), "2 pills, hard-wrapped", 26, 40);
}

/// The card does not scroll, and the header block can now push `rotate at` off
/// a short pane entirely (two pills, each dragging a fix line that wraps). The
/// caret must NOT be placed then: `set_cursor_position` takes an absolute row,
/// and a real terminal clamps an out-of-range one onto the last line rather
/// than dropping it — parking a visible caret on a border or the pane below.
#[test]
fn typed_threshold_caret_is_not_set_when_the_row_is_clipped_off_the_pane() {
    let mut d = profile("a", 95.0, 10.0, 7200);
    d.disabled = true;
    let mut cfg = config_with(vec![d], Some("other"), vec!["a"]);
    cfg.state.auth_broken.push("a".into());

    let mut app = App::new(cfg);
    app.fallback_focus = FallbackFocus::Detail;
    app.fallback_detail_cursor = FALLBACK_ROWS
        .iter()
        .position(|r| *r == FallbackRow::Threshold)
        .unwrap();
    app.fallback_threshold_draft = Some(InputState::new("80"));

    // 26x17: the two wrapped fix lines push the rows past the pane's last line.
    let (w, h) = (26u16, 17u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();

    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    assert!(
        !rows.iter().any(|r| r.contains("rotate at")),
        "fixture must actually clip the row, else this pins nothing:\n{}",
        rows.join("\n")
    );
    let caret = term.get_cursor_position().unwrap();
    assert!(
        (caret.y as usize) < h as usize,
        "caret must never be set past the terminal's last row (y={}, h={h})",
        caret.y
    );
}

/// The Fallback detail card keeps BOTH facts for a disabled member: the
/// `[ disabled ]` pill says the operator excluded it, the health pill beneath
/// says it is also broken. Before this, `Disabled` ranking first meant the card
/// showed only the exclusion and the dead login was invisible tab-wide.
#[test]
fn member_detail_stacks_the_health_pill_under_disabled() {
    let mut d = profile("a", 95.0, 10.0, 7200);
    d.disabled = true;
    let mut cfg = config_with(vec![d], Some("other"), vec!["a"]);
    cfg.state.auth_broken.push("a".into());

    // Both pills on one `├│└` rail, each with its own fix line. The first row
    // carries the `status` key so the rail has a column to anchor against; the
    // second bridges with `│` at col 0 while the rail is still open.
    let (lines, _) = member_detail(&cfg, "a", false, 0, false, None, None, 60, None);
    let block: Vec<String> = lines.iter().take(4).map(line_text).collect();
    assert_eq!(
        block,
        vec![
            "status       [ disabled ]".to_string(),
            "├ excluded from the walk, enable it on the setup tab".to_string(),
            "│            [ auth broken ]".to_string(),
            "└ re-login with clauth login a".to_string(),
        ],
        "both facts stack on one rail, each naming its own fix"
    );

    // An ENABLED but auth-broken member is unchanged: one pill, lone `└`.
    let mut e = profile("a", 95.0, 10.0, 7200);
    e.disabled = false;
    let mut enabled = config_with(vec![e], Some("other"), vec!["a"]);
    enabled.state.auth_broken.push("a".into());
    let (lines, _) = member_detail(&enabled, "a", false, 0, false, None, None, 60, None);
    assert_eq!(
        lines.iter().take(2).map(line_text).collect::<Vec<_>>(),
        vec![
            "status       [ auth broken ]".to_string(),
            "└ re-login with clauth login a".to_string(),
        ],
        "a single pill stays a lone `└` — nothing to connect"
    );
}

/// The selector rail shows `#n` and keeps a CONSTANT width across one- and
/// two-digit positions, so every name in the list starts on the same column.
/// A ragged rail is what a bare `{}` would give, and it is invisible in any
/// test that only renders a short chain.
#[test]
fn selector_rail_shows_hash_n_at_constant_width() {
    // 10 members so the list spans `#1` through `#10`. Names are deliberately
    // non-prefixing (`ma`..`mj`, never `m1`..`m10`): `m1` is a substring of `m10`
    // and `#1` of `#10`, so a naive `contains` lookup would match the wrong row
    // and then measure a column that happens to agree anyway.
    let names: Vec<String> = (b'a'..=b'j').map(|c| format!("m{}", c as char)).collect();
    let profiles: Vec<_> = names.iter().map(|n| profile(n, 95.0, 10.0, 7200)).collect();
    let chain: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let cfg = config_with(profiles, None, chain);
    let app = App::new(cfg);

    let (w, h) = (100u16, 20u16);
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    // Both panes share every physical row and the detail card repeats the
    // selected member's name in its TITLE (on a border row, which carries no
    // `││` seam to split on) — so identify selector rows by the rail's own `#`
    // rather than by name, and measure the name column within those.
    // Walk forward from the rail's own `#` (past its digits and the gap) to find
    // where the name starts. Searching the row for the name STRING instead would
    // collide with the detail pane sharing the same physical row — `max spend`
    // contains `ma`, which silently reported a name column of 34.
    let rows = crate::testutil::buffer_rows(term.backend().buffer());
    let mut name_cols = Vec::new();
    for row in &rows {
        let chars: Vec<char> = row.chars().collect();
        let Some(hash) = chars.iter().position(|c| *c == '#') else {
            continue;
        };
        let mut i = hash + 1;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        // Our members are the only `m?` tokens that can follow a rail ordinal.
        if chars.get(i) == Some(&'m') {
            name_cols.push(i);
        }
    }
    assert_eq!(
        name_cols.len(),
        names.len(),
        "every member must render a `#n` selector row; got {name_cols:?}"
    );
    assert_eq!(
        name_cols
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1,
        "every name must start on the same column; got {name_cols:?}"
    );

    // …and the ordinals really are `#n`, spanning both digit widths — otherwise
    // a rail that dropped the `#` entirely would still pass the column check.
    let joined = rows.join("\n");
    assert!(joined.contains("#1"), "first position renders as #1");
    assert!(joined.contains("#10"), "tenth position renders as #10");
}

/// Every `reason_fix` arm must be reachable and non-empty: a match arm that can
/// never fire is dead copy, and an empty hint renders a bare `└` with nothing
/// after it. Enumerates one representative of each variant.
#[test]
fn every_reason_fix_variant_is_reachable_and_non_empty() {
    let all = [
        BlockedReason::Disabled,
        BlockedReason::Canceled,
        BlockedReason::AuthBroken,
        BlockedReason::WeeklySpent { resets_in: None },
        BlockedReason::KickRejected { lifts_in: 60 },
        BlockedReason::BudgetSpent,
        BlockedReason::FiveHour {
            pct: 99.0,
            resets_in: None,
        },
        BlockedReason::WeeklySoft { pct: 85.0 },
        BlockedReason::Stale,
    ];
    let mut seen: Vec<String> = Vec::new();
    for reason in all {
        let fix = reason_fix(&reason, "acct");
        assert!(!fix.trim().is_empty(), "{reason:?} has no fix copy");
        assert_eq!(
            fix,
            fix.to_lowercase(),
            "{reason:?} fix must stay lowercase like every other hint: {fix}"
        );
        assert!(
            !seen.contains(&fix),
            "{reason:?} reuses another variant's copy ({fix}) — the arm is indistinguishable"
        );
        seen.push(fix);
    }
    assert_eq!(seen.len(), 9, "every variant contributed a distinct fix");
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

    let off = member_detail(&cfg, "a", true, 2, false, None, None, 60, None).0;
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
    let on = member_detail(&cfg, "a", true, 2, false, None, None, 60, None).0;
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
