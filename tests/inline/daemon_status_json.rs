#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `daemon::status_json::build_status` shape + field derivation.
//!
//! These exercise the single-shot path (`live = None`, freshness/next-refresh
//! from cache mtime) against a `HomeSandbox` so no real `~/.clauth` is touched.

use super::*;
use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, save_profile};
use crate::testutil::HomeSandbox;

fn oauth_profile(name: &str) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: format!("{name}-access"),
            refresh_token: Some(format!("{name}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: Some("max".to_string()),
        }),
    });
    p
}

#[test]
fn build_status_top_level_shape_and_active() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work"), oauth_profile("home")],
    };
    config.state.active_profile = Some("work".into());
    config.state.refresh_interval_ms = 300_000;

    let v = build_status(&config, config.state.refresh_interval_ms, None);

    assert_eq!(v["schema"], SCHEMA_VERSION);
    assert_eq!(v["active_profile"], "work");
    assert_eq!(v["wrap_off"], false);
    assert_eq!(v["refresh_interval_ms"], 300_000);
    assert!(v["generated_at"].as_str().unwrap().contains('T'));
    // Exact key sets — a silent rename/removal anywhere in the contract fails
    // here rather than in a downstream reader.
    let mut top: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    top.sort_unstable();
    assert_eq!(
        top,
        [
            "active_profile",
            "generated_at",
            "pending_switch",
            "profiles",
            "refresh_interval_ms",
            "schema",
            "wrap_off",
        ],
    );
    let profiles = v["profiles"].as_array().unwrap();
    let mut per: Vec<&str> = profiles[0]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    per.sort_unstable();
    assert_eq!(
        per,
        [
            "active",
            "auth_status",
            "auto_start",
            "base_url",
            "bell_threshold",
            "fallback",
            "fetch_status",
            "fetched_at",
            "has_live_session",
            "name",
            "next_refresh_at",
            "provider",
            "stale",
            "third_party",
            "tier",
            "windows",
        ],
    );
    assert_eq!(profiles.len(), 2);
    let work = profiles.iter().find(|p| p["name"] == "work").unwrap();
    assert_eq!(work["active"], true);
    assert_eq!(work["provider"], "anthropic");
    // No cache on disk → never-fetched profile reports nulls, not stale numbers.
    assert!(work["fetch_status"].is_null());
    assert!(work["fetched_at"].is_null());
    assert!(work["next_refresh_at"].is_null());
    assert!(work["windows"].as_array().unwrap().is_empty());
    let home = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "home")
        .unwrap()
        .clone();
    assert_eq!(home["active"], false);
}

#[test]
fn build_status_fallback_membership_and_armed() {
    let _home = HomeSandbox::new();
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("a"), oauth_profile("b"), oauth_profile("c")],
    };
    for p in &config.profiles {
        save_profile(p).unwrap();
    }
    config.state.active_profile = Some("a".into());
    config.state.fallback_chain = vec!["a".into(), "b".into()];

    let v = build_status(&config, 300_000, None);
    let profiles = v["profiles"].as_array().unwrap();

    let a = profiles.iter().find(|p| p["name"] == "a").unwrap();
    assert_eq!(a["fallback"]["position"], 1);
    assert_eq!(a["fallback"]["threshold"], 95.0); // DEFAULT_THRESHOLD
    assert_eq!(a["fallback"]["armed"], true, "active + in chain = armed");

    let b = profiles.iter().find(|p| p["name"] == "b").unwrap();
    assert_eq!(b["fallback"]["position"], 2);
    assert_eq!(b["fallback"]["armed"], false, "in chain but not active");

    let c = profiles.iter().find(|p| p["name"] == "c").unwrap();
    assert!(c["fallback"].is_null(), "not a chain member → null");
}

// ── AUTH-2: auth_status + pending_switch contract ─────────────────────────────

fn set_expiry(p: &mut Profile, expires_at: i64) {
    p.credentials
        .as_mut()
        .unwrap()
        .claude_ai_oauth
        .as_mut()
        .unwrap()
        .expires_at = Some(expires_at);
}

#[test]
fn build_status_auth_status_ok_expiring_broken() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms() as i64;

    let mut ok = oauth_profile("ok");
    set_expiry(&mut ok, now + 3_600_000); // real life left → ok
    let mut expiring = oauth_profile("expiring");
    set_expiry(&mut expiring, now - 1_000); // past due, not flagged → expiring
    let mut broken = oauth_profile("broken");
    set_expiry(&mut broken, now - 1_000); // past due AND flagged → broken wins

    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![ok, expiring, broken],
    };
    config.set_auth_broken("broken", true);

    let v = build_status(&config, 300_000, None);
    let profiles = v["profiles"].as_array().unwrap();
    let get = |n: &str| profiles.iter().find(|p| p["name"] == n).unwrap();
    assert_eq!(get("ok")["auth_status"], "ok");
    assert_eq!(get("expiring")["auth_status"], "expiring");
    assert_eq!(
        get("broken")["auth_status"],
        "broken",
        "broken outranks expiring"
    );
}

/// `auth_status` reports on the credential a profile STORES, not on where its
/// requests route: a hybrid (OAuth pair + `base_url`) with a dead access token
/// must publish `expiring`, while an endpoint-only profile has no token to expire.
#[test]
fn build_status_auth_status_types_the_hybrid_on_its_credential() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms() as i64;

    let mut hybrid = oauth_profile("hybrid");
    set_expiry(&mut hybrid, now - 1_000);
    hybrid.base_url = Some("https://api.z.ai/api/anthropic".to_string());

    let api_key_only = Profile::new(
        "apikey".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-test".to_string()),
    );

    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![hybrid, api_key_only],
    };

    let v = build_status(&config, 300_000, None);
    let profiles = v["profiles"].as_array().unwrap();
    let get = |n: &str| profiles.iter().find(|p| p["name"] == n).unwrap();
    assert_eq!(
        get("hybrid")["auth_status"],
        "expiring",
        "a stored pair expires regardless of the endpoint it routes past"
    );
    assert_eq!(
        get("apikey")["auth_status"],
        "ok",
        "no stored pair → nothing to expire"
    );
}

#[test]
fn build_status_pending_switch_reflects_live_signal() {
    let _home = HomeSandbox::new();
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work")],
    };
    let empty_status = std::collections::HashMap::new();
    let empty_next = std::collections::HashMap::new();
    let empty_streaks = std::collections::HashMap::new();

    // single-shot (no daemon) → pending_switch is present-but-null.
    let none = build_status(&config, 300_000, None);
    assert!(
        none.get("pending_switch").is_some(),
        "pending_switch key is always present"
    );
    assert!(none["pending_switch"].is_null());

    let live = LiveSignals {
        status: &empty_status,
        next_refresh: &empty_next,
        streaks: &empty_streaks,
        pending_switch: Some("home"),
    };
    let v = build_status(&config, 300_000, Some(&live));
    assert_eq!(v["pending_switch"], "home");
    assert_eq!(
        v["schema"], SCHEMA_VERSION,
        "pending_switch is part of schema 1 — no bump"
    );
}

/// An api-key profile's freshness derives from ITS cache
/// (`THIRD_PARTY_CACHE_FILE`), and a name the live stores don't carry falls
/// back to the same derivation — pre-fix both keyed on the OAuth
/// `USAGE_CACHE_FILE`/status store, so a healthy hourly-refreshed api-key
/// account rendered permanently as never-fetched (`fetch_status: null`).
#[test]
fn build_status_third_party_freshness_from_its_own_cache() {
    let _home = HomeSandbox::new();
    let mut api = Profile::new("zai".to_string(), None, None);
    api.base_url = Some("https://api.z.ai/api/anthropic".to_string());
    api.api_key = Some("k".to_string());
    api.provider = crate::providers::Provider::from_base_url(api.base_url.as_deref().unwrap());
    assert!(api.is_third_party(), "fixture must be an api-key profile");
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![api],
    };

    // Warm third-party cache, no OAuth cache: the profile is fetched.
    crate::profile_cache::write_profile_cache(
        "zai",
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &crate::providers::ThirdPartyStats {
            is_available: true,
            rows: vec![],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
        },
    );

    // Single-shot: freshness from the third-party cache mtime (just written).
    let v = build_status(&config, 300_000, None);
    let p = &v["profiles"].as_array().unwrap()[0];
    assert_eq!(p["fetch_status"], "Fresh");
    assert!(!p["fetched_at"].is_null());
    assert!(!p["next_refresh_at"].is_null());
    assert_eq!(p["third_party"]["available"], true);

    // Live daemon whose stores don't carry the name (the OAuth-leg stores
    // never do for api-key profiles): same derivation, not null.
    let empty_status = std::collections::HashMap::new();
    let empty_next = std::collections::HashMap::new();
    let empty_streaks = std::collections::HashMap::new();
    let live = LiveSignals {
        status: &empty_status,
        next_refresh: &empty_next,
        streaks: &empty_streaks,
        pending_switch: None,
    };
    let v = build_status(&config, 300_000, Some(&live));
    let p = &v["profiles"].as_array().unwrap()[0];
    assert_eq!(
        p["fetch_status"], "Fresh",
        "a live daemon must not blank an api-key profile's freshness"
    );
    assert!(!p["next_refresh_at"].is_null());
}

// RLS-1: the additive per-profile `stale` flag = the daemon distrusts this
// reading as a deep-slot stuck RateLimited (live status RateLimited AND the 429
// streak past the active cap) — the SAME predicate `scan_auto_switch` acts on,
// so the published cue and the switch decision cannot drift. Additive: schema
// stays 1; the single-shot (no streaks) is always false.
#[test]
fn build_status_stale_flags_a_deep_slot_stuck_rate_limited_profile() {
    use crate::usage::FetchStatus;
    use std::collections::HashMap;

    let _home = HomeSandbox::new();
    // TWO profiles, so a "computed once and applied to every row" regression
    // (rather than keyed per profile name) is catchable.
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("work"), oauth_profile("home")],
    };
    let next: HashMap<String, u64> = HashMap::new();
    let deep = crate::usage::ACTIVE_CAP_MAX_STREAK + 1;
    let stale_of = |name: &str, v: &serde_json::Value| -> serde_json::Value {
        v["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .unwrap()["stale"]
            .clone()
    };

    // single-shot (no daemon / no streaks) → stale is present-and-false.
    let none = build_status(&config, 300_000, None);
    assert_eq!(
        none["schema"], 1,
        "stale is additive — schema must not bump"
    );
    assert_eq!(
        stale_of("work", &none),
        false,
        "single-shot never publishes a distrusted reading"
    );

    // Two profiles in ONE body: `work` is a deep-slot stuck RateLimited (→ stale),
    // `home` is Fresh with an (irrelevant) equally-deep streak (→ NOT stale). This
    // one call proves the flag keys on the profile's OWN status+streak, is
    // per-profile (not one value smeared across the array), and that streak depth
    // alone never stales a live reading.
    let status = HashMap::from([
        ("work".to_string(), FetchStatus::RateLimited),
        ("home".to_string(), FetchStatus::Fresh),
    ]);
    let streaks = HashMap::from([("work".to_string(), deep), ("home".to_string(), deep)]);
    let live = LiveSignals {
        status: &status,
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
    };
    let v = build_status(&config, 300_000, Some(&live));
    assert_eq!(
        stale_of("work", &v),
        true,
        "a deep-slot stuck RateLimited reading is published as stale"
    );
    assert_eq!(
        stale_of("home", &v),
        false,
        "a Fresh sibling is never stale however deep its streak — and stale is \
         per-profile, not computed once and applied to the whole array"
    );

    // Shallow RateLimited (≤ cap) → not yet distrusted.
    let status = HashMap::from([("work".to_string(), FetchStatus::RateLimited)]);
    let streaks = HashMap::from([("work".to_string(), crate::usage::ACTIVE_CAP_MAX_STREAK)]);
    let live = LiveSignals {
        status: &status,
        next_refresh: &next,
        streaks: &streaks,
        pending_switch: None,
    };
    let v = build_status(&config, 300_000, Some(&live));
    assert_eq!(
        stale_of("work", &v),
        false,
        "a shallow RateLimited reading is not stale"
    );
}
