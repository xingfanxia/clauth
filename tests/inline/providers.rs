//! Inline tests for `crate::providers` — provider URL matching and the
//! disk-cache roundtrip. DeepSeek-specific mapping tests live in
//! `providers_deepseek.rs`.

use super::*;

// ── Provider::from_base_url ───────────────────────────────────────────────────

#[test]
fn deepseek_matches_exact_base_url() {
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.com"),
        Some(Provider::DeepSeek)
    );
}

#[test]
fn deepseek_matches_base_url_with_path() {
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.com/v1"),
        Some(Provider::DeepSeek)
    );
}

#[test]
fn deepseek_rejects_host_extension() {
    // A bare prefix match would claim these and send the profile's API key
    // to the real provider endpoint.
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.com.evil.tld"),
        None
    );
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.community"),
        None
    );
}

#[test]
fn deepseek_rejects_plain_http_and_unrelated_hosts() {
    assert_eq!(Provider::from_base_url("http://api.deepseek.com"), None);
    assert_eq!(Provider::from_base_url("https://api.anthropic.com"), None);
    assert_eq!(Provider::from_base_url(""), None);
}

#[test]
fn deepseek_matches_uppercase_host() {
    // Hosts are case-insensitive (RFC 3986) — a profile pasted with caps still
    // resolves to the provider rather than falling through to "plain API".
    assert_eq!(
        Provider::from_base_url("https://API.DeepSeek.com/v1"),
        Some(Provider::DeepSeek)
    );
}

#[test]
fn deepseek_matches_explicit_port() {
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.com:443/v1"),
        Some(Provider::DeepSeek)
    );
}

// ── Disk cache ────────────────────────────────────────────────────────────────

use crate::testutil::HomeSandbox;

#[test]
fn disk_cache_roundtrips_stats() {
    let _home = HomeSandbox::new();
    let stats = ThirdPartyStats {
        is_available: true,
        rows: vec![StatRow {
            label: "total".to_string(),
            value: "110.00 USD".to_string(),
            kind: StatRowKind::Body,
        }],
        bars: Vec::new(),
        plan: None,
        endpoint: None,
        best_effort: false,
    };
    crate::profile_cache::write_profile_cache(
        "tp-cache-test",
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &stats,
    );
    let loaded = crate::profile_cache::load_profile_cache::<ThirdPartyStats>(
        "tp-cache-test",
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("cache present");
    assert!(loaded.is_available);
    assert_eq!(loaded.rows.len(), 1);
    assert_eq!(loaded.rows[0].value, "110.00 USD");
}

#[test]
fn disk_cache_missing_reads_as_none() {
    let _home = HomeSandbox::new();
    assert!(
        crate::profile_cache::load_profile_cache::<ThirdPartyStats>(
            "tp-cache-absent",
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        )
        .is_none()
    );
}

// ── api_origin ───────────────────────────────────────────────────────────────

#[test]
fn api_origin_strips_path_to_scheme_host() {
    assert_eq!(
        api_origin("https://api.z.ai/api/anthropic").as_deref(),
        Some("https://api.z.ai")
    );
    assert_eq!(
        api_origin("https://api.deepseek.com/v1").as_deref(),
        Some("https://api.deepseek.com")
    );
}

#[test]
fn api_origin_keeps_port_drops_query_and_fragment() {
    assert_eq!(
        api_origin("https://host.example:8443/path?x=1#frag").as_deref(),
        Some("https://host.example:8443")
    );
}

#[test]
fn api_origin_none_without_scheme_delimiter() {
    assert!(api_origin("api.z.ai/usage").is_none());
}

// ── ThirdPartyTarget::throttle_key ─────────────────────────────────────────────

#[test]
fn throttle_key_known_provider_uses_canonical_origin() {
    // Distinct providers key distinct hosts so they pace independently.
    assert_eq!(
        ThirdPartyTarget::Known(Provider::DeepSeek).throttle_key(),
        "https://api.deepseek.com"
    );
    assert_eq!(
        ThirdPartyTarget::Known(Provider::Zai).throttle_key(),
        "https://api.z.ai"
    );
}

#[test]
fn throttle_key_generic_strips_to_origin() {
    // Two api-key profiles on the same host collapse to one pacing key (serialize);
    // a different host yields a different key (parallel).
    assert_eq!(
        ThirdPartyTarget::Generic {
            base_url: "https://proxy.example/v1".to_string(),
        }
        .throttle_key(),
        "https://proxy.example"
    );
}

#[test]
fn throttle_key_generic_falls_back_to_raw_when_schemeless() {
    // No `://` to parse an origin from — the raw base URL is still a stable key.
    assert_eq!(
        ThirdPartyTarget::Generic {
            base_url: "localhost:1234".to_string(),
        }
        .throttle_key(),
        "localhost:1234"
    );
}
