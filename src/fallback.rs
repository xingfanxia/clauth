use anyhow::Result;

use crate::actions::switch_profile;
use crate::lock::with_state_lock;
use crate::profile::{AppConfig, Profile};

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

/// Picks the next chain member to switch to, starting one slot after the
/// active profile and wrapping. Returns None when nothing is viable.
///
/// Two passes:
///   1. Any member with real headroom (5h utilization below threshold, or no
///      usage data fetched yet).
///   2. As a last resort, a member with threshold == 100% — accepted even
///      while it's at 100%. Claude Code will show its own "out of 5h limit"
///      message on arrival.
fn next_target(config: &AppConfig) -> Option<String> {
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

    walk(&|p| !is_exhausted(p)).or_else(|| walk(&|p| threshold_for(p) >= 100.0))
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
