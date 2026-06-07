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

/// RAII home sandbox: holds `HOME_TEST_LOCK` and redirects `home_dir()` into a
/// tempdir for the test's duration, clearing on drop (even on panic).
struct HomeSandbox {
    // Drop order: tempdir first, then shared lock.
    _tmp: tempfile::TempDir,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl HomeSandbox {
    fn new() -> Self {
        let guard = crate::profile::HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("create home sandbox");
        crate::profile::set_home_override(tmp.path().to_path_buf());
        Self {
            _tmp: tmp,
            _guard: guard,
        }
    }
}

impl Drop for HomeSandbox {
    fn drop(&mut self) {
        crate::profile::clear_home_override();
    }
}

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
    };
    write_third_party_disk_cache("tp-cache-test", &stats);
    let loaded = load_third_party_disk_cache("tp-cache-test").expect("cache present");
    assert!(loaded.is_available);
    assert_eq!(loaded.rows.len(), 1);
    assert_eq!(loaded.rows[0].value, "110.00 USD");
}

#[test]
fn disk_cache_missing_reads_as_none() {
    let _home = HomeSandbox::new();
    assert!(load_third_party_disk_cache("tp-cache-absent").is_none());
}
