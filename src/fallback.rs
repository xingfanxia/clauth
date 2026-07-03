use anyhow::Result;

use crate::actions::{switch_off, switch_profile};
use crate::lock::with_state_lock;
use crate::profile::{AppConfig, Profile};
use crate::usage::{UsageStore, five_hour_live, now_epoch_secs};

/// What the auto-switch evaluator decided when the active profile crossed its
/// threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwitchAction {
    /// Switch the active profile to this chain member.
    To(String),
    /// Turn off all accounts: clear the live credentials and unset the active
    /// profile. Emitted only in wrap-off mode when the whole chain is exhausted
    /// and no 100%-threshold sink exists (every threshold is below 100%).
    Off,
}

/// Default 5-hour utilization threshold (percent) applied when a chain member
/// has no per-profile override.
pub(crate) const DEFAULT_THRESHOLD: f64 = 95.0;

pub(crate) fn threshold_for(profile: &Profile) -> f64 {
    profile.fallback_threshold.unwrap_or(DEFAULT_THRESHOLD)
}

/// True when the profile's 5h utilization has crossed its own threshold.
/// Only a live window (future `resets_at`) can exhaust — a lapsed or windowless
/// snapshot means the account has headroom again whatever its last-known
/// utilization says (`five_hour_live`, the same reading the auto-start leg uses).
/// Also drives the TUI's all-spent banner wording.
pub(crate) fn is_exhausted(profile: &Profile) -> bool {
    let Some(usage) = profile.usage.as_ref() else {
        return false;
    };
    if !five_hour_live(usage, now_epoch_secs()) {
        return false;
    }
    usage
        .five_hour
        .as_ref()
        .is_some_and(|w| w.utilization >= threshold_for(profile))
}

/// One chain member as observed when a `ChainSnapshot` was built. Holds enough
/// to evaluate the fallback decision without re-locking `AppConfig` — caller
/// snapshots once under the config mutex then reads `UsageStore` separately,
/// avoiding the `config ↔ store` lock inversion against `App::apply_usage`
/// (which holds `usage_store` then takes `config`).
#[derive(Debug, Clone)]
pub(crate) struct ChainMember {
    pub(crate) name: String,
    pub(crate) threshold: f64,
}

/// In-memory snapshot of the fields `next_auto_switch_target` needs: active
/// profile name + ordered chain with each member's resolved threshold. Built
/// under the `AppConfig` mutex by [`snapshot_chain`], then evaluated lock-free.
#[derive(Debug, Clone)]
pub(crate) struct ChainSnapshot {
    pub(crate) active: String,
    pub(crate) chain: Vec<ChainMember>,
    /// Snapshot of `AppState::wrap_off` — drives the switch-off-all decision.
    pub(crate) wrap_off: bool,
}

/// Snapshot active profile + chain + per-member thresholds out of `AppConfig`.
/// Returns `None` when there's no active profile, the active isn't a chain
/// member, or the chain is empty — every case where `next_auto_switch_target`
/// short-circuits anyway, so callers can skip evaluation on `None`.
pub(crate) fn snapshot_chain(config: &AppConfig) -> Option<ChainSnapshot> {
    let active = config.state.active_profile.as_deref()?.to_string();
    let chain = &config.state.fallback_chain;
    if !chain.iter().any(|n| n == &active) {
        return None;
    }
    let chain = chain
        .iter()
        .map(|name| ChainMember {
            name: name.to_string(),
            threshold: config
                .find(name)
                .map(threshold_for)
                .unwrap_or(DEFAULT_THRESHOLD),
        })
        .collect();
    Some(ChainSnapshot {
        active,
        chain,
        wrap_off: config.state.wrap_off,
    })
}

/// Scheduler-side [`is_exhausted`]: reads 5h utilization from the shared
/// `UsageStore` rather than `Profile.usage` (which only the UI thread writes via
/// `apply_usage`). A poisoned store lock fails safe to "not exhausted" so a
/// momentarily wedged mutex can't trigger a switch.
fn is_exhausted_from_store(name: &str, threshold: f64, store: &UsageStore) -> bool {
    let now = now_epoch_secs();
    match store.lock() {
        Ok(s) => s.get(name).is_some_and(|info| {
            five_hour_live(info, now)
                && info
                    .five_hour
                    .as_ref()
                    .is_some_and(|w| w.utilization >= threshold)
        }),
        Err(_) => false,
    }
}

/// Chain walk shared by [`next_target`] and [`next_auto_switch_target`]. Scans
/// every other slot starting one after `idx` and wrapping. `skip_pred(i)` skips
/// slots (active profile, or a member with no resolvable profile);
/// `accept_pred(i)` selects the first matching slot, whose index is returned.
fn walk_chain(
    idx: usize,
    len: usize,
    skip_pred: &dyn Fn(usize) -> bool,
    accept_pred: &dyn Fn(usize) -> bool,
) -> Option<usize> {
    for offset in 1..=len {
        let i = (idx + offset) % len;
        if skip_pred(i) {
            continue;
        }
        if accept_pred(i) {
            return Some(i);
        }
    }
    None
}

/// Picks the next chain member to switch to, starting one slot after the active
/// profile and wrapping. Returns None when nothing is viable.
///
///   1. Any member with real headroom (5h utilization below threshold, or no
///      usage data fetched yet).
///   2. Last resort: a member with threshold == 100%, accepted even at 100%.
///      Claude Code shows its own "out of 5h limit" message on arrival.
///   3. Wrap-off only: no headroom, no sink (every threshold < 100%), and the
///      active profile itself exhausted → [`SwitchAction::Off`] to halt usage.
pub(crate) fn next_target(config: &AppConfig) -> Option<SwitchAction> {
    let active = config.state.active_profile.as_deref()?;
    let chain = &config.state.fallback_chain;
    let active_idx = chain.iter().position(|n| n == active)?;
    let len = chain.len();

    let skip = |i: usize| chain[i] == active || config.find(&chain[i]).is_none();
    let walk = |accept: &dyn Fn(&Profile) -> bool| -> Option<String> {
        let pick = walk_chain(active_idx, len, &skip, &|i| {
            config.find(&chain[i]).is_some_and(&accept)
        });
        pick.map(|i| chain[i].to_string())
    };

    if let Some(name) = walk(&|p| !is_exhausted(p)) {
        return Some(SwitchAction::To(name));
    }

    // Only fall back to a 100%-threshold sink when the active profile is NOT
    // itself such a sink. Two maxed sinks switching to each other indefinitely
    // gains nothing — one migration is fine, but the next tick must stay put.
    let active_is_sink = config
        .find(active)
        .is_some_and(|p| threshold_for(p) >= 100.0);
    if active_is_sink {
        return None;
    }
    if let Some(name) = walk(&|p| threshold_for(p) >= 100.0) {
        return Some(SwitchAction::To(name));
    }

    // No headroom, no sink anywhere (every threshold < 100%). In wrap-off mode,
    // turn off all accounts — but only when the active profile is itself
    // exhausted, since this picker is also exercised on a healthy active.
    if config.state.wrap_off && config.find(active).is_some_and(is_exhausted) {
        return Some(SwitchAction::Off);
    }
    None
}

/// Scheduler-side [`auto_switch_if_needed`]: same logic over an in-memory
/// [`ChainSnapshot`] taken under the config mutex, reading utilization from the
/// shared `UsageStore`. Returns the member to switch to, or `None`.
///
/// The store/config lock split is load-bearing: `App::apply_usage` locks
/// `usage_store` then `config`, so the scheduler must never hold `config` while
/// taking `usage_store`. Caller builds the snapshot under `config.lock()`, drops
/// the guard, then calls this.
pub(crate) fn next_auto_switch_target(
    snapshot: &ChainSnapshot,
    store: &UsageStore,
) -> Option<SwitchAction> {
    let active_idx = snapshot
        .chain
        .iter()
        .position(|m| m.name == snapshot.active)?;
    let len = snapshot.chain.len();

    let active = &snapshot.chain[active_idx];
    if !is_exhausted_from_store(&active.name, active.threshold, store) {
        return None;
    }

    let skip = |i: usize| snapshot.chain[i].name == active.name;
    let walk = |accept: &dyn Fn(&ChainMember) -> bool| -> Option<String> {
        let pick = walk_chain(active_idx, len, &skip, &|i| accept(&snapshot.chain[i]));
        pick.map(|i| snapshot.chain[i].name.clone())
    };

    if let Some(name) = walk(&|m| !is_exhausted_from_store(&m.name, m.threshold, store)) {
        return Some(SwitchAction::To(name));
    }

    let active_is_sink = active.threshold >= 100.0;
    if active_is_sink {
        return None;
    }
    if let Some(name) = walk(&|m| m.threshold >= 100.0) {
        return Some(SwitchAction::To(name));
    }

    // No headroom, no sink anywhere (every threshold < 100%), and the active
    // profile is already exhausted (gated above). In wrap-off mode, halt all
    // usage instead of staying on the spent profile.
    if snapshot.wrap_off {
        return Some(SwitchAction::Off);
    }
    None
}

/// Find the first chain member whose utilization is below its threshold
/// (has recovered headroom after switch-off-all). Returns the member name.
/// Safe to call without holding the config lock — reads from [`UsageStore`].
pub(crate) fn find_recovered_member(chain: &[ChainMember], store: &UsageStore) -> Option<String> {
    let now = now_epoch_secs();
    for member in chain {
        // A fetched entry whose 5h window is absent or past its reset is idle
        // headroom; a live window recovers only below the member's threshold.
        // An absent entry (never fetched) stays undecidable.
        let recovered = match store.lock() {
            Ok(s) => s.get(&member.name).map(|info| {
                !five_hour_live(info, now)
                    || info
                        .five_hour
                        .as_ref()
                        .is_none_or(|w| w.utilization < member.threshold)
            }),
            Err(_) => None,
        };
        if recovered == Some(true) {
            return Some(member.name.clone());
        }
    }
    None
}

/// If the active profile is a chain member past its threshold, switch to the
/// next viable member — or, in wrap-off mode when the whole chain is spent and
/// no sink exists, turn off all accounts. Returns the action taken, or None.
pub(crate) fn auto_switch_if_needed(config: &mut AppConfig) -> Result<Option<SwitchAction>> {
    with_state_lock(|| {
        let Some(active_name) = config.state.active_profile.as_deref() else {
            return Ok(None);
        };
        if !config.state.fallback_chain.iter().any(|n| n == active_name) {
            return Ok(None);
        }
        let Some(active) = config.find(active_name) else {
            return Ok(None);
        };
        if !is_exhausted(active) {
            return Ok(None);
        }

        let Some(action) = next_target(config) else {
            return Ok(None);
        };

        match &action {
            SwitchAction::To(target) => switch_profile(config, target)?,
            SwitchAction::Off => switch_off(config)?,
        }
        Ok(Some(action))
    })
}

#[cfg(test)]
#[path = "../tests/inline/fallback.rs"]
mod tests;
