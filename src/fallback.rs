use anyhow::Result;

use crate::actions::switch_profile;
use crate::lock::with_state_lock;
use crate::profile::{AppConfig, Profile};
use crate::usage::UsageStore;

/// Default 5-hour utilization threshold (percent) applied when a chain member
/// has no per-profile override.
pub(crate) const DEFAULT_THRESHOLD: f64 = 95.0;

pub(crate) fn threshold_for(profile: &Profile) -> f64 {
    profile.fallback_threshold.unwrap_or(DEFAULT_THRESHOLD)
}

/// True when the profile's 5h utilization has crossed its own threshold.
fn is_exhausted(profile: &Profile) -> bool {
    let Some(window) = profile.usage.as_ref().and_then(|u| u.five_hour.as_ref()) else {
        return false;
    };
    window.utilization >= threshold_for(profile)
}

/// One chain member as observed at the moment a `ChainSnapshot` was built.
/// Holds enough to evaluate the fallback decision without re-locking
/// `AppConfig` — caller snapshots once under the config mutex and then
/// reads `UsageStore` separately, avoiding the `config ↔ store` lock
/// inversion against `App::apply_usage` (which holds `usage_store` then
/// takes `config`).
#[derive(Debug, Clone)]
pub(crate) struct ChainMember {
    pub(crate) name: String,
    pub(crate) threshold: f64,
}

/// In-memory snapshot of just the fields `next_auto_switch_target` needs:
/// the active profile name and the ordered chain with each member's
/// resolved threshold. Built under the `AppConfig` mutex by
/// [`snapshot_chain`], then evaluated lock-free.
#[derive(Debug, Clone)]
pub(crate) struct ChainSnapshot {
    pub(crate) active: String,
    pub(crate) chain: Vec<ChainMember>,
}

/// Snapshot the active profile + fallback chain + per-member thresholds
/// out of `AppConfig`. Returns `None` when there's no active profile, the
/// active isn't a chain member, or the chain is empty — every case where
/// `next_auto_switch_target` would short-circuit anyway, so callers can
/// skip the chain evaluation entirely on `None`.
pub(crate) fn snapshot_chain(config: &AppConfig) -> Option<ChainSnapshot> {
    let active = config.state.active_profile.as_deref()?.to_string();
    let chain = &config.state.fallback_chain;
    if !chain.iter().any(|n| n == &active) {
        return None;
    }
    let chain = chain
        .iter()
        .map(|name| ChainMember {
            name: name.clone(),
            threshold: config
                .find(name)
                .map(threshold_for)
                .unwrap_or(DEFAULT_THRESHOLD),
        })
        .collect();
    Some(ChainSnapshot { active, chain })
}

/// True when the profile's 5h utilization (read from the shared `UsageStore`)
/// has crossed `threshold`. Scheduler-side equivalent of [`is_exhausted`] —
/// reads from the store rather than `Profile.usage`, which only the UI
/// thread writes via `apply_usage`. A poisoned store lock fails safe to
/// "not exhausted" so a momentarily wedged mutex can't trigger a switch.
fn is_exhausted_from_store(name: &str, threshold: f64, store: &UsageStore) -> bool {
    let util = match store.lock() {
        Ok(s) => s
            .get(name)
            .and_then(|u| u.five_hour.as_ref())
            .map(|w| w.utilization),
        Err(_) => return false,
    };
    let Some(util) = util else {
        return false;
    };
    util >= threshold
}

/// Picks the next chain member to switch to, starting one slot after the
/// active profile and wrapping. Returns None when nothing is viable.
///
/// Two passes:
///   1. Any member with real headroom (5h utilization below threshold, or no
///      usage data fetched yet).
///   2. As a last resort, a member with threshold == 100% — accepted even
///      while it's at 100%. Claude Code will show its own "out of 5h limit"
///      message on arrival.
pub(crate) fn next_target(config: &AppConfig) -> Option<String> {
    let active = config.state.active_profile.as_deref()?;
    let chain = &config.state.fallback_chain;
    let active_idx = chain.iter().position(|n| n == active)?;
    let len = chain.len();

    let walk = |accept: &dyn Fn(&Profile) -> bool| -> Option<String> {
        for offset in 1..=len {
            let candidate = &chain[(active_idx + offset) % len];
            if candidate == active {
                continue;
            }
            let Some(profile) = config.find(candidate) else {
                continue;
            };
            if accept(profile) {
                return Some(candidate.clone());
            }
        }
        None
    };

    // Only fall back to a 100%-threshold sink when the active profile is NOT
    // itself such a sink. Two maxed sinks switching to each other indefinitely
    // gains nothing — one migration is fine, but the next tick must stay put.
    let active_is_sink = config
        .find(active)
        .is_some_and(|p| threshold_for(p) >= 100.0);

    walk(&|p| !is_exhausted(p)).or_else(|| {
        if active_is_sink {
            return None;
        }
        walk(&|p| threshold_for(p) >= 100.0)
    })
}

/// Scheduler-side decision: same logic as [`auto_switch_if_needed`] but
/// operates on an in-memory [`ChainSnapshot`] taken under the config
/// mutex, and reads utilization from the shared `UsageStore`. Returns the
/// chain member to switch to, or `None` when no action is warranted.
///
/// The store/config lock split is load-bearing: `App::apply_usage` locks
/// `usage_store` then `config`, so the scheduler must never hold `config`
/// while taking `usage_store`. Caller is expected to build the snapshot
/// under `config.lock()`, drop the guard, then call this.
pub(crate) fn next_auto_switch_target(
    snapshot: &ChainSnapshot,
    store: &UsageStore,
) -> Option<String> {
    let active_idx = snapshot
        .chain
        .iter()
        .position(|m| m.name == snapshot.active)?;
    let len = snapshot.chain.len();

    let active = &snapshot.chain[active_idx];
    if !is_exhausted_from_store(&active.name, active.threshold, store) {
        return None;
    }

    let walk = |accept: &dyn Fn(&ChainMember) -> bool| -> Option<String> {
        for offset in 1..=len {
            let candidate = &snapshot.chain[(active_idx + offset) % len];
            if candidate.name == active.name {
                continue;
            }
            if accept(candidate) {
                return Some(candidate.name.clone());
            }
        }
        None
    };

    let active_is_sink = active.threshold >= 100.0;
    walk(&|m| !is_exhausted_from_store(&m.name, m.threshold, store)).or_else(|| {
        if active_is_sink {
            return None;
        }
        walk(&|m| m.threshold >= 100.0)
    })
}

/// If the active profile is a chain member and its 5h utilization has crossed
/// its threshold, switch to the next viable chain member. Returns the name
/// switched to, or None when no action was taken.
pub(crate) fn auto_switch_if_needed(config: &mut AppConfig) -> Result<Option<String>> {
    with_state_lock(|| {
        let active_name = config.state.active_profile.clone();
        let Some(active_name) = active_name else {
            return Ok(None);
        };
        if !config
            .state
            .fallback_chain
            .iter()
            .any(|n| n == &active_name)
        {
            return Ok(None);
        }
        let Some(active) = config.find(&active_name) else {
            return Ok(None);
        };
        if !is_exhausted(active) {
            return Ok(None);
        }

        let Some(target) = next_target(config) else {
            return Ok(None);
        };

        switch_profile(config, &target)?;
        Ok(Some(target))
    })
}

#[cfg(test)]
#[path = "../tests/inline/fallback.rs"]
mod tests;
