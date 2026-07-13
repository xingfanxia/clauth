//! `~/.clauth/status.json` serializer — the daemon's published feed, and the
//! shape `clauth status --json` prints (one code path builds both, so they
//! cannot drift). Contract: wiki/daemon.md.
//!
//! Usage windows/tier come from the on-disk `usage_cache.json` (written by the
//! scheduler), so this is process-independent: it returns the last-persisted
//! numbers whether or not a scheduler is live. Two fields — `fetch_status` and
//! `next_refresh_at` — live only in the scheduler's in-memory stores; when a
//! live daemon passes [`LiveSignals`] they come from there, otherwise they are
//! derived from the cache-file mtime so the single-shot `status --json` still
//! produces a coherent shape.

use std::collections::HashMap;

use crate::profile::{AppConfig, Profile};
use crate::profile_cache::{
    THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache, profile_cache_mtime_ms,
};
use crate::profile_json::{provider_label, tier_label, windows_json};
use crate::providers::ThirdPartyStats;
use crate::usage::{FetchStatus, epoch_secs_to_iso, is_stuck_rate_limited, now_ms};

/// Bump when the JSON shape changes in a way readers must branch on.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// Live scheduler signals a running daemon has that the single-shot
/// `clauth status --json` cannot see. When absent, freshness and next-refresh
/// are derived from the cache-file mtime instead.
///
/// These are already-snapshotted plain maps, not the live `Arc<RankedMutex<…>>`
/// stores: the caller must snapshot the status/next-refresh stores and release
/// those locks *before* taking the `config` lock and calling [`build_status`],
/// because `config` (rank CONFIG) outranks `USAGE_STATUS` — reading the live
/// stores while holding `config` would invert lock order.
pub(crate) struct LiveSignals<'a> {
    pub(crate) status: &'a HashMap<String, FetchStatus>,
    pub(crate) next_refresh: &'a HashMap<String, u64>,
    /// Consecutive-429 streaks, so a profile whose live `status` is `RateLimited`
    /// AND whose streak has passed the active cap can be published as `stale` (a
    /// deep-slot stuck read the daemon distrusts — the same judgment
    /// `scan_auto_switch` acts on). Empty for the single-shot `status --json` (no
    /// daemon), so `stale` is always `false` there.
    pub(crate) streaks: &'a HashMap<String, u32>,
    /// The switch target the daemon has accepted but not yet applied (from
    /// `pending_switch`), so a reader can show in-flight truth instead of a
    /// timing heuristic. `None` for the single-shot `status --json` (no daemon).
    pub(crate) pending_switch: Option<&'a str>,
}

fn fetch_status_str(s: FetchStatus) -> &'static str {
    match s {
        FetchStatus::Fresh => "Fresh",
        FetchStatus::Cached => "Cached",
        FetchStatus::Failed => "Failed",
        FetchStatus::RateLimited => "RateLimited",
    }
}

/// ISO-8601 (UTC) from an epoch-millisecond instant.
fn iso_from_ms(ms: u64) -> String {
    epoch_secs_to_iso((ms / 1000) as i64)
}

/// The `fallback` object for a profile, or `None` when it is not a chain member.
/// `armed` = in the chain AND currently active (the account auto-switch would
/// rotate away from). `position` is 1-based.
fn fallback_json(config: &AppConfig, p: &Profile) -> Option<serde_json::Value> {
    let name = p.name.as_str();
    let pos = config
        .state
        .fallback_chain
        .iter()
        .position(|n| n.as_str() == name)?;
    Some(serde_json::json!({
        "position": pos + 1,
        "threshold": crate::fallback::threshold_for(p),
        "armed": config.is_active(name),
    }))
}

/// Per-profile auth health for `status.json`. `broken` (last refresh rejected
/// as revoked/invalid — `AppState::auth_broken`) outranks `expiring` (an OAuth
/// access token past its expiry, refresh not yet run); everything else is
/// `ok`. Readers default an absent field to `ok` (the additive-evolution
/// rule); it is still emitted for an explicit, greppable contract.
fn auth_status_str(config: &AppConfig, p: &Profile, now_ms: i64) -> &'static str {
    if config.is_auth_broken(p.name.as_str()) {
        return "broken";
    }
    if p.is_oauth() && p.access_token_expires_at().is_some_and(|exp| now_ms >= exp) {
        return "expiring";
    }
    "ok"
}

/// Build the full `status.json` body. `interval_ms` is the live refresh interval
/// (daemon) or `config.state.refresh_interval_ms` (single-shot). `live` carries
/// the scheduler's in-memory freshness/countdown stores when a daemon is running.
pub(crate) fn build_status(
    config: &AppConfig,
    interval_ms: u64,
    live: Option<&LiveSignals>,
) -> serde_json::Value {
    let now = now_ms();
    let profiles: Vec<serde_json::Value> = config
        .profiles
        .iter()
        .map(|p| {
            let name = p.name.as_str();
            // Freshness reads each profile's OWN cache: the third-party leg
            // never touches USAGE_CACHE_FILE, so keying api-key profiles on it
            // rendered a healthy hourly-refreshed account as never-fetched.
            let cache_file = if p.is_third_party() {
                THIRD_PARTY_CACHE_FILE
            } else {
                USAGE_CACHE_FILE
            };
            let mtime_ms = profile_cache_mtime_ms(name, cache_file);

            // fetch_status: the live store when a daemon is running, else
            // derive from cache freshness (Fresh within one interval, else
            // Cached). The live store only carries the OAuth leg's outcomes,
            // so a name missing from it (api-key profiles, a just-started
            // daemon) falls back to the same derivation rather than reading
            // as never-fetched; null = no cache at all.
            let derived_status = || {
                mtime_ms.map(|mt| {
                    if now.saturating_sub(mt) < interval_ms {
                        "Fresh"
                    } else {
                        "Cached"
                    }
                })
            };
            let fetch_status: Option<&'static str> = match live {
                Some(sig) => sig
                    .status
                    .get(name)
                    .copied()
                    .map(fetch_status_str)
                    .or_else(derived_status),
                None => derived_status(),
            };

            // next_refresh_at: the live countdown store, else mtime + interval
            // (also the fallback for names the live store doesn't carry).
            let derived_next = || mtime_ms.map(|mt| mt.saturating_add(interval_ms));
            let next_refresh_ms: Option<u64> = match live {
                Some(sig) => sig.next_refresh.get(name).copied().or_else(derived_next),
                None => derived_next(),
            };

            // `stale` = the daemon distrusts this reading — a deep-slot stuck
            // RateLimited (live status RateLimited AND the 429 streak past the
            // active cap). Read from the LIVE store, not the mtime-derived
            // fetch_status string (which never yields RateLimited), so it is only
            // ever true under a real daemon. The single-shot has no streaks →
            // always false. Same predicate `scan_auto_switch` distrusts, so the
            // published flag and the switch decision cannot drift.
            let stale = match live {
                Some(sig) => sig.status.get(name).copied().is_some_and(|s| {
                    is_stuck_rate_limited(s, sig.streaks.get(name).copied().unwrap_or(0))
                }),
                None => false,
            };

            // Structured third-party balance isn't carried by ThirdPartyStats
            // (it lives in free-text `rows`); expose only the availability flag
            // for now — enough for a reader's red/green reachability dot.
            let third_party = if p.is_third_party() {
                load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE)
                    .map(|s| serde_json::json!({ "available": s.is_available }))
            } else {
                None
            };

            serde_json::json!({
                "name": name,
                "active": config.is_active(name),
                "provider": provider_label(p),
                "base_url": p.base_url,
                "tier": tier_label(p),
                "has_live_session": crate::runtime::has_live_session(name),
                "auth_status": auth_status_str(config, p, now as i64),
                "fetch_status": fetch_status,
                // Additive (schema stays 1): true when the daemon distrusts this
                // reading as a deep-slot stuck RateLimited — readers dim it / show
                // a "stuck" cue instead of treating it as current truth. Always
                // false for the single-shot `status --json`.
                "stale": stale,
                "fetched_at": mtime_ms.map(iso_from_ms),
                "next_refresh_at": next_refresh_ms.map(iso_from_ms),
                "auto_start": p.auto_start,
                "bell_threshold": p.bell_threshold,
                "fallback": fallback_json(config, p),
                "windows": windows_json(name),
                "third_party": third_party,
            })
        })
        .collect();

    serde_json::json!({
        "schema": SCHEMA_VERSION,
        "generated_at": iso_from_ms(now),
        "active_profile": config.state.active_profile.as_deref(),
        "pending_switch": live.and_then(|s| s.pending_switch),
        "wrap_off": config.state.wrap_off,
        "refresh_interval_ms": interval_ms,
        "profiles": profiles,
    })
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_status_json.rs"]
mod tests;
