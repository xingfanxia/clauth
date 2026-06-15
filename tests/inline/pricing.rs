//! Inline tests for `crate::pricing` — distill parsing, rate lookup with suffix
//! fallback, and per-model cost math. No network: every test builds a table from
//! a literal so the pure logic is exercised deterministically.

use super::*;

use std::collections::HashMap;

/// Build a `PriceTable` from `(id, input, output, cache_read, cache_write)` rows.
fn table(rows: &[(&str, f64, f64, f64, f64)]) -> PriceTable {
    let mut rates = HashMap::new();
    for &(id, input, output, cache_read, cache_write) in rows {
        rates.insert(
            id.to_owned(),
            ModelRate {
                input,
                output,
                cache_read,
                cache_write,
            },
        );
    }
    PriceTable {
        rates,
        fetched_at_ms: 0,
    }
}

fn model(id: &str, input: u64, output: u64, cache_read: u64, cache_create: u64) -> ModelTokens {
    ModelTokens {
        model: id.to_owned(),
        input,
        output,
        cache_read,
        cache_create,
    }
}

// ── distill ────────────────────────────────────────────────────────────────

#[test]
fn distill_keeps_priced_bare_keys_and_drops_paths() {
    let json = r#"{
        "claude-opus-4-8": {
            "input_cost_per_token": 0.000005,
            "output_cost_per_token": 0.000025,
            "cache_read_input_token_cost": 0.0000005,
            "cache_creation_input_token_cost": 0.00000625
        },
        "deepseek-chat": {
            "input_cost_per_token": 0.00000028,
            "output_cost_per_token": 0.00000042,
            "cache_read_input_token_cost": 0.000000028,
            "cache_creation_input_token_cost": null
        },
        "bedrock/us-east-1/claude-opus-4-8": {
            "input_cost_per_token": 0.000005
        },
        "some-embedding-model": {
            "output_cost_per_token": 0.0
        }
    }"#;

    let rates = distill(json).expect("distill ok");

    // Bare priced key kept with all four buckets.
    let opus = rates.get("claude-opus-4-8").expect("opus present");
    assert_eq!(opus.input, 0.000005);
    assert_eq!(opus.output, 0.000025);
    assert_eq!(opus.cache_read, 0.0000005);
    assert_eq!(opus.cache_write, 0.00000625);

    // Null cache-write defaults to 0.0 (e.g. DeepSeek auto-cache).
    let ds = rates.get("deepseek-chat").expect("deepseek present");
    assert_eq!(ds.cache_write, 0.0);

    // Path-style keys dropped; entries without input cost dropped.
    assert!(!rates.contains_key("bedrock/us-east-1/claude-opus-4-8"));
    assert!(!rates.contains_key("some-embedding-model"));
}

#[test]
fn distill_rejects_non_object_root() {
    assert!(distill("[]").is_err());
    assert!(distill("not json").is_err());
}

// ── rate lookup ──────────────────────────────────────────────────────────────

#[test]
fn rate_exact_match() {
    let t = table(&[("claude-opus-4-8", 5e-6, 25e-6, 5e-7, 6.25e-6)]);
    assert_eq!(t.rate("claude-opus-4-8").map(|r| r.input), Some(5e-6));
}

#[test]
fn rate_strips_trailing_date_stamp() {
    let t = table(&[("claude-sonnet-4-5", 3e-6, 15e-6, 3e-7, 3.75e-6)]);
    // CC logs a date-stamped id; falls back to the bare family-version key.
    assert_eq!(
        t.rate("claude-sonnet-4-5-20250929").map(|r| r.output),
        Some(15e-6)
    );
}

#[test]
fn rate_strips_variant_suffix() {
    let t = table(&[("claude-opus-4-6", 5e-6, 25e-6, 5e-7, 6.25e-6)]);
    assert_eq!(
        t.rate("claude-opus-4-6-thinking").map(|r| r.input),
        Some(5e-6)
    );
}

#[test]
fn rate_unknown_model_is_none() {
    let t = table(&[("claude-opus-4-8", 5e-6, 25e-6, 5e-7, 6.25e-6)]);
    assert!(t.rate("gpt-5").is_none());
    assert!(t.rate("others").is_none());
}

#[test]
fn rate_fallback_never_matches_bare_family_key() {
    // A bare `claude` key must not wildcard-match every `claude-*` variant via
    // the suffix-strip fallback — only an exact lookup reaches it.
    let t = table(&[("claude", 1e-6, 2e-6, 0.0, 0.0)]);
    assert!(t.rate("claude-sonnet-4-5-20250929").is_none());
    assert_eq!(t.rate("claude").map(|r| r.input), Some(1e-6)); // exact still works
}

// ── cost ───────────────────────────────────────────────────────────────────

#[test]
fn cost_sums_all_four_buckets() {
    // Clean rates: $1/$2/$0.10/$1.25 per million.
    let t = table(&[("m", 1e-6, 2e-6, 1e-7, 1.25e-6)]);
    let m = model("m", 1_000_000, 1_000_000, 1_000_000, 1_000_000);
    // 1.0 + 2.0 + 0.10 + 1.25 = 4.35
    let c = t.cost(&m).expect("priced");
    assert!((c - 4.35).abs() < 1e-9, "got {c}");
}

#[test]
fn cost_none_for_unpriced_model() {
    let t = table(&[("m", 1e-6, 2e-6, 1e-7, 1.25e-6)]);
    assert!(t.cost(&model("unknown", 1000, 0, 0, 0)).is_none());
}

#[test]
fn total_cost_counts_unpriced_with_tokens() {
    let t = table(&[("m", 1e-6, 2e-6, 0.0, 0.0)]);
    let models = vec![
        model("m", 1_000_000, 0, 0, 0),     // $1.00, priced
        model("unknown", 500_000, 0, 0, 0), // unpriced, has tokens → counted
        model("empty-unknown", 0, 0, 0, 0), // unpriced, no tokens → ignored
    ];
    let (total, unpriced) = t.total_cost(&models);
    assert!((total - 1.0).abs() < 1e-9, "got {total}");
    assert_eq!(unpriced, 1);
}
