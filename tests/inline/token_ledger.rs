use super::*;

use crate::tokens::{DayModelTokens, ModelTokens, TokenStats};

// ── helpers ──────────────────────────────────────────────────────────────────

fn split(model: &str, input: u64, output: u64, cache_read: u64, cache_create: u64) -> ModelTokens {
    ModelTokens {
        model: model.to_owned(),
        input,
        output,
        cache_read,
        cache_create,
    }
}

/// A merged base carrying transcript-derived (split-bearing) per-day rows — the
/// shape `merge_topup` hands to [`Ledger::record`].
fn base_with(days: &[(&str, ModelTokens)]) -> TokenStats {
    let mut b = TokenStats::default();
    for (date, m) in days {
        b.daily_models.push(DayModelTokens {
            date: (*date).to_owned(),
            model: m.model.clone(),
            in_out: m.in_out(),
            split: Some(m.clone()),
        });
    }
    b
}

// ── durability ────────────────────────────────────────────────────────────────

/// The core guarantee: a day recorded from transcripts is fully reconstructable
/// afterwards even when the base froze earlier and the transcripts are gone.
#[test]
fn record_then_apply_survives_transcript_loss() {
    let m = split("claude-opus-4-8", 100, 50, 2000, 500);
    let merged = base_with(&[("2026-06-16", m)]);

    let mut ledger = Ledger::default();
    assert!(ledger.record(&merged, "2026-06-18"));
    assert_eq!(ledger.recorded_through.as_deref(), Some("2026-06-17"));

    // Fresh base as if stats-cache is frozen at 06-09 and 06-16's transcripts
    // have been pruned — the ledger is the only surviving record.
    let mut fresh = TokenStats::default();
    ledger.apply_to_base(&mut fresh, Some("2026-06-09"));

    assert_eq!(fresh.total_input, 100);
    assert_eq!(fresh.total_output, 50);
    assert_eq!(fresh.total_cache_read, 2000);
    assert_eq!(fresh.total_cache_create, 500);
    assert_eq!(fresh.daily.len(), 1);
    assert_eq!(fresh.daily[0].date, "2026-06-16");
    assert_eq!(fresh.daily[0].tokens, 150, "daily is in+out");
    assert_eq!(fresh.daily_models.len(), 1);
    let row = &fresh.daily_models[0];
    assert_eq!(row.model, "claude-opus-4-8");
    assert_eq!(row.in_out, 150);
    assert_eq!(row.split.as_ref().expect("split kept").cache_read, 2000);
    assert_eq!(fresh.models.len(), 1);
    assert_eq!(fresh.models[0].total(), 100 + 50 + 2000 + 500);
}

/// A day CC's own aggregation later catches up to (base advances past it) must
/// not be folded twice.
#[test]
fn apply_skips_days_covered_by_stats_cache() {
    let mut ledger = Ledger::default();
    ledger.record(
        &base_with(&[("2026-06-16", split("claude-opus-4-8", 100, 50, 0, 0))]),
        "2026-06-18",
    );

    let mut fresh = TokenStats::default();
    ledger.apply_to_base(&mut fresh, Some("2026-06-16"));

    assert!(
        fresh.daily.is_empty(),
        "a day at/before lastComputedDate is the base's, never re-added"
    );
    assert!(fresh.models.is_empty());
    assert_eq!(fresh.total_input, 0);
}

// ── cutoff ────────────────────────────────────────────────────────────────────

#[test]
fn effective_cutoff_takes_the_later_boundary() {
    let mut l = Ledger::default();
    assert_eq!(
        l.effective_cutoff(Some("2026-06-09")).as_deref(),
        Some("2026-06-09"),
        "no ledger yet → the base date"
    );
    assert_eq!(l.effective_cutoff(None), None);

    l.record(
        &base_with(&[("2026-06-16", split("m", 1, 0, 0, 0))]),
        "2026-06-18",
    );
    // recorded_through = 06-17.
    assert_eq!(
        l.effective_cutoff(Some("2026-06-09")).as_deref(),
        Some("2026-06-17"),
        "ledger past a frozen base bounds the sweep"
    );
    assert_eq!(
        l.effective_cutoff(Some("2026-07-01")).as_deref(),
        Some("2026-07-01"),
        "a base that advanced past the ledger wins"
    );
    assert_eq!(l.effective_cutoff(None).as_deref(), Some("2026-06-17"));
}

// ── record boundaries ──────────────────────────────────────────────────────────

#[test]
fn record_never_stores_today() {
    let today = "2026-06-17";
    let merged = base_with(&[
        ("2026-06-16", split("m", 10, 0, 0, 0)),
        (today, split("m", 999, 0, 0, 0)),
    ]);

    let mut l = Ledger::default();
    assert!(l.record(&merged, today));
    assert!(l.days.contains_key("2026-06-16"));
    assert!(
        !l.days.contains_key(today),
        "today is still being written — never finalized"
    );
    assert_eq!(l.recorded_through.as_deref(), Some("2026-06-16"));
}

#[test]
fn watermark_advances_across_idle_days_and_is_monotonic() {
    let mut l = Ledger::default();
    // A run with no usage still finalizes every day through yesterday.
    assert!(l.record(&TokenStats::default(), "2026-06-20"));
    assert_eq!(l.recorded_through.as_deref(), Some("2026-06-19"));
    assert!(l.days.is_empty());

    // A later day already at/before the watermark records nothing and never
    // regresses the watermark.
    let old = base_with(&[("2026-06-10", split("m", 5, 0, 0, 0))]);
    assert!(!l.record(&old, "2026-06-20"));
    assert!(l.days.is_empty());
    assert_eq!(l.recorded_through.as_deref(), Some("2026-06-19"));
}

// ── persistence ────────────────────────────────────────────────────────────────

#[test]
fn save_load_round_trip() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).expect("mkdir .clauth");

    let mut l = Ledger::default();
    l.record(
        &base_with(&[("2026-06-16", split("claude-opus-4-8", 7, 3, 100, 20))]),
        "2026-06-18",
    );
    l.save(&dir);

    let reloaded = Ledger::load(&dir);
    assert_eq!(reloaded.recorded_through.as_deref(), Some("2026-06-17"));
    let mut fresh = TokenStats::default();
    reloaded.apply_to_base(&mut fresh, Some("2026-06-01"));
    assert_eq!(fresh.total_input, 7);
    assert_eq!(fresh.total_cache_read, 100);
}

/// A missing ledger is an empty one, never an error.
#[test]
fn load_missing_is_empty() {
    let sb = crate::testutil::HomeSandbox::new();
    let l = Ledger::load(&sb.home().join(".clauth"));
    assert!(l.recorded_through.is_none());
    assert!(l.days.is_empty());
}
