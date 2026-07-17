//! CDX-5 account pool + selection (proxy-design.md §1.5/§1.6). Pure over an
//! in-memory snapshot so the sticky/cooldown/skip logic is exhaustively
//! table-tested; the token-freshness resolution and store IO live behind an
//! injectable trait so tests never touch HTTP or the real `~/.codex`.

use std::collections::HashMap;

/// One pool member the selector may route to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PoolMember {
    pub(crate) name: String,
    /// Epoch-ms until which this account is in cooldown after a 429 (its
    /// advertised reset, or a 60s floor). `0` = available now.
    pub(crate) cooldown_until_ms: u64,
    /// True when the member can't currently serve — auth_broken, leased to an
    /// isolated session, or exhausted by cached usage. Skipped like cooldown
    /// but without a timer.
    pub(crate) unavailable: bool,
}

/// The selector's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Selection {
    /// Route this request to `name`.
    Use(String),
    /// Every member is in cooldown or unavailable — nothing to route to.
    Exhausted,
}

/// Pick the account for a request (proxy-design §1.5): sticky to the active
/// codex profile when it is available, else the first available member in
/// chain order starting AFTER the active (the CDX-4 walk order), wrapping.
/// `now_ms` gates cooldowns. Pure.
pub(crate) fn select_account(
    ordered: &[PoolMember],
    active: Option<&str>,
    now_ms: u64,
) -> Selection {
    let available = |m: &PoolMember| !m.unavailable && now_ms >= m.cooldown_until_ms;

    // Sticky: stay on the active account while it can serve (prompt-cache
    // affinity — the prior-art lesson).
    if let Some(active) = active
        && ordered.iter().any(|m| m.name == active && available(m))
    {
        return Selection::Use(active.to_string());
    }

    // Rotation: walk from just after the active, wrapping, to the first
    // available member (chain order = the CDX-4 walk order).
    let start = active
        .and_then(|a| ordered.iter().position(|m| m.name == a))
        .map(|i| i + 1)
        .unwrap_or(0);
    let len = ordered.len();
    for offset in 0..len {
        let m = &ordered[(start + offset) % len];
        if available(m) {
            return Selection::Use(m.name.clone());
        }
    }
    Selection::Exhausted
}

/// The next account to try after `current` failed pre-commit (429/401/5xx
/// before the first byte) — the same walk as [`select_account`] but excluding
/// `current` and any member in `already_tried` (so one request walks each
/// member at most once). Pure.
pub(crate) fn next_after_failure(
    ordered: &[PoolMember],
    current: &str,
    already_tried: &[String],
    now_ms: u64,
) -> Selection {
    let start = ordered
        .iter()
        .position(|m| m.name == current)
        .map(|i| i + 1)
        .unwrap_or(0);
    let len = ordered.len();
    for offset in 0..len {
        let m = &ordered[(start + offset) % len];
        if m.name == current
            || m.unavailable
            || now_ms < m.cooldown_until_ms
            || already_tried.iter().any(|t| t == &m.name)
        {
            continue;
        }
        return Selection::Use(m.name.clone());
    }
    Selection::Exhausted
}

/// Cooldown state shared across the proxy's connection threads — per-account
/// "in cooldown until" stamps set on a 429. A plain mutex-guarded map; the
/// proxy holds it briefly per selection/failure, never across IO.
#[derive(Debug, Default)]
pub(crate) struct Cooldowns {
    until_ms: HashMap<String, u64>,
}

/// Minimum cooldown after a 429 with no advertised reset (proxy-design §1.5:
/// the ≥60s rotation guardrail).
pub(crate) const COOLDOWN_FLOOR_MS: u64 = 60 * 1000;

impl Cooldowns {
    pub(crate) fn get(&self, name: &str) -> u64 {
        self.until_ms.get(name).copied().unwrap_or(0)
    }

    /// Stamp `name` in cooldown until `reset_ms` (an advertised reset), or the
    /// [`COOLDOWN_FLOOR_MS`] floor from `now_ms` — whichever is later.
    pub(crate) fn stamp(&mut self, name: &str, now_ms: u64, reset_ms: Option<u64>) {
        let floor = now_ms.saturating_add(COOLDOWN_FLOOR_MS);
        let until = reset_ms.map(|r| r.max(floor)).unwrap_or(floor);
        self.until_ms.insert(name.to_string(), until);
    }
}

#[cfg(test)]
#[path = "../../tests/inline/proxy_pool.rs"]
mod tests;
