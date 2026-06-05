use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::claude::{LinkState, classify_credentials_link};
use crate::lock::with_state_lock;
use crate::profile::{
    AppConfig, OAuthToken, clear_staged_credentials, save_app_state, save_profile,
    stage_rotated_credentials,
};
use crate::runtime::{RotationGuard, has_live_session};
use crate::usage::{
    ActivityKind, ActivityStore, OpResult, OpResultSender, ProfileActivity, RefetchQueue,
    UsageStore, clear_activity, iso_to_epoch_secs, mark_activity, now_epoch_secs, now_ms,
};

/// Anthropic's OAuth token endpoint. Same one Claude Code uses on startup to
/// mint an access token from the stored refresh token.
const TOKEN_ENDPOINT: &str = "https://api.anthropic.com/v1/oauth/token";

/// UUID of the "Claude Code" OAuth application; required for refresh.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Minimal inference endpoint we use to "kick" the 5-hour usage window.
/// Token refresh alone does NOT start the timer — only a real `/v1/messages`
/// call does. Probing with `count_tokens`, `oauth/usage`, or session
/// endpoints all confirmed this experimentally.
const MESSAGES_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// Cheapest available model — single token costs ~0.001¢.
const KICK_MODEL: &str = "claude-haiku-4-5-20251001";

/// OAuth tokens require the "Claude Code" system prefix or the server rejects
/// the call as an unauthorized non-CC inference.
const KICK_SYSTEM_PROMPT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Refuse to re-ping a profile until this long after its last SUCCESSFUL
/// auto-start. Sized just under the 5-hour window so we don't re-kick a profile
/// that just opened one. Claimed pre-kick (concurrency guard) and kept on
/// success; overwritten to a shorter backoff on failure.
const AUTO_START_COOLDOWN_MS: u64 = 4 * 3600 * 1000 + 30 * 60 * 1000;

/// How long to wait before retrying after a failed kick. Short enough to
/// recover from a transient API error or a just-rotated token, but not so
/// short that a persistent failure hammers the endpoint.
const AUTO_START_RETRY_MS: u64 = 5 * 60 * 1000;

#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_in: u64,
    #[serde(default)]
    pub(crate) scope: Option<String>,
}

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        .timeout_recv_response(Some(Duration::from_secs(15)))
        .build()
        .into()
});

pub(crate) fn refresh(refresh_token: &str) -> Result<TokenResponse> {
    let body = serde_json::to_string(&serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    }))?;

    let text = AGENT
        .post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .send(&body)
        .map_err(crate::ureq_error::into_anyhow)?
        .body_mut()
        .read_to_string()
        .map_err(crate::ureq_error::into_anyhow)?;

    serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{e}: {text}"))
}

/// Sends a 1-token Haiku message to start the 5-hour usage window. Mirrors what
/// Claude Code does silently on launch.
fn kick(access_token: &str) -> Result<()> {
    let body = serde_json::to_string(&serde_json::json!({
        "model": KICK_MODEL,
        "max_tokens": 1,
        "system": [{ "type": "text", "text": KICK_SYSTEM_PROMPT }],
        "messages": [{ "role": "user", "content": "x" }],
    }))?;

    let status = AGENT
        .post(MESSAGES_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .send(&body)
        .map_err(crate::ureq_error::into_anyhow)?
        .status()
        .as_u16();
    if status >= 400 {
        return Err(anyhow::anyhow!("HTTP {status}"));
    }
    Ok(())
}

/// Result of [`rotate_one_inner`]. Distinguishes the rotation-lock acquire
/// failure (no `OpResult` emitted, no activity pre-stamp to clear) from every
/// other path (which emits its own `OpResult` and clears activity). Lets
/// `refresh_all` workers surface the guard-fail as a Danger toast.
enum RotateOutcome {
    /// `RotationGuard::acquire` failed — a live session or sibling worker holds
    /// the per-profile rotation lock. No `OpResult` was emitted.
    GuardBusy,
    /// The HTTP/persist leg ran and emitted its `OpResult`. The bool is whether
    /// the rotated pair was persisted.
    Persisted(bool),
}

/// Body of each [`refresh_all`] worker. Holds the per-profile rotation lock
/// across the ENTIRE HTTP window so an external `clauth start <name>` cannot
/// begin a refresh of the same single-use token while ours is in flight (the
/// state flock can't — it must release across the round trip). Ordering rule
/// (matches `ProfileRuntime::acquire`): RotationGuard OUTERMOST, then state
/// flock inside. With the guard held, the `has_live_session` check below is
/// authoritative, not a TOCTOU probe: a session that won the race stamped its
/// PID file before releasing the guard; one that lost is blocked here until we
/// finish and persist.
///
/// `force` bypasses ONLY the `has_live_session` SKIP (user explicitly wants
/// every account rotated, including one a live session touches); it never
/// relaxes the mutual exclusion, still serialised against that session's own
/// refresh of the same chain.
///
/// HTTP/persist leg emits one `OpResult { kind: Refreshing }` and clears the
/// activity slot. Returns [`RotateOutcome::GuardBusy`] without emitting an
/// `OpResult` when the lock can't be acquired (slot never pre-stamped here;
/// `refresh_all` pre-stamps and clears it). No-refresh-token / skipped-live-
/// session legs return [`RotateOutcome::Persisted(false)`].
fn rotate_one_inner(
    config: &crate::profile::ConfigHandle,
    name: &str,
    activity: Option<&ActivityStore>,
    sender: &OpResultSender,
    force: bool,
) -> RotateOutcome {
    let Ok(_rotation_guard) = RotationGuard::acquire(name) else {
        return RotateOutcome::GuardBusy;
    };
    let token = {
        let cfg = config.lock().expect("config mutex poisoned");
        with_state_lock(|| {
            if !force && has_live_session(name) {
                return Ok::<_, anyhow::Error>(None);
            }
            let rt = cfg
                .find(name)
                .and_then(|p| p.refresh_token().map(str::to_string));
            if rt.is_some()
                && let Some(activity) = activity
            {
                // Stamp Refreshing under the state lock so partition_due cannot
                // observe this profile as Idle between the credential read and
                // the HTTP call. Lock order (AppConfig → state → leaf) is preserved:
                // activity is a leaf mutex acquired inside with_state_lock.
                mark_activity(activity, name, ProfileActivity::Refreshing);
            }
            Ok(rt)
        })
        .ok()
        .flatten()
    };

    let Some(rt) = token else {
        return RotateOutcome::Persisted(false);
    };
    let outcome = refresh(&rt).and_then(|tok| apply_rotated_tokens_locked(config, name, tok));
    let applied = outcome.is_ok();
    if let Some(activity) = activity {
        clear_activity(activity, name);
    }
    let _ = sender.send(OpResult {
        name: name.to_string(),
        kind: ActivityKind::Refreshing,
        outcome,
    });
    RotateOutcome::Persisted(applied)
}

/// Profiles `refresh_all` would rotate, as `(name, refresh_token)` pairs.
/// Extracted so tests can pin the inclusion logic without the network.
/// Diverged-active and live-session profiles are included only when `force`.
pub(crate) fn rotation_candidates(config: &AppConfig, force: bool) -> Vec<(String, String)> {
    // force=true (t-key rotate-all) bypasses diverged-active: user wants every
    // account rotated, including the one CC is touching.
    let skip_active = !force && active_link_diverged(config);
    config
        .profiles
        .iter()
        .filter_map(|p| {
            if skip_active && config.is_active(&p.name) {
                return None;
            }
            if !force && has_live_session(&p.name) {
                return None;
            }
            Some((p.name.to_string(), p.refresh_token()?.to_string()))
        })
        .collect()
}

/// Refreshes every profile's OAuth token pair (rotated pair saved to disk).
/// Mirrors what Claude Code does silently on launch — minus the kick.
///
/// Profiles without a stored refresh token are skipped. Network/revocation
/// failures are swallowed per-profile; cached state stays put. `force` bypasses
/// both the `has_live_session` and diverged-active guards.
///
/// Returns the names whose rotation succeeded so the caller can target
/// follow-up work (re-fetch, kick) at the same set, and pushes each onto
/// `refetch` so the next tick re-fetches usage without waiting for the cadence.
///
/// Takes `&ConfigHandle` so per-profile workers lock/unlock independently around
/// their HTTP calls, never holding the config mutex across the network. Each
/// worker emits one `OpResult` on `sender` the moment its HTTP completes, so the
/// spinner clears in arrival order, not when the slowest sibling finishes.
pub(crate) fn refresh_all(
    config: &crate::profile::ConfigHandle,
    force: bool,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
    sender: &OpResultSender,
) -> Vec<String> {
    let snapshots = {
        let cfg = config.lock().expect("config mutex poisoned");
        rotation_candidates(&cfg, force)
    };

    if snapshots.is_empty() {
        return Vec::new();
    }

    // Stamp every candidate Refreshing before the fan-out so the overview row
    // shows a refresh spinner for the entire window. Each worker clears its
    // own slot when it emits its OpResult so the spinner drops as soon as
    // that profile's HTTP returns, not when the slowest sibling does.
    for (name, _) in &snapshots {
        mark_activity(activity, name, ProfileActivity::Refreshing);
    }

    // Pair each handle with the name so the join loop can clear the activity
    // slot on panic — the closure consumes the name, so we keep a second copy.
    let handles: Vec<(String, _)> = snapshots
        .into_iter()
        .map(|(name, _rt)| {
            let config = Arc::clone(config);
            let activity = Arc::clone(activity);
            let sender = sender.clone();
            let name_for_handle = name.clone();
            let h = std::thread::spawn(move || {
                // Holds the per-profile RotationGuard across the HTTP window so
                // an external `clauth start <name>` cannot double-spend this
                // single-use token mid-rotation. `force` bypasses the
                // `has_live_session` SKIP but NOT the mutual exclusion: a forced
                // rotate must still not race a live session's own refresh.
                let outcome = rotate_one_inner(&config, &name, Some(&activity), &sender, force);
                (name, outcome)
            });
            (name_for_handle, h)
        })
        .collect();

    let mut refreshed = Vec::new();
    for (name, h) in handles {
        match h.join() {
            Ok((n, RotateOutcome::Persisted(true))) => refreshed.push(n),
            // Guard-fail leg never emits an OpResult, so this pre-stamped slot
            // would freeze the spinner AND swallow the failure. Emit the Danger
            // toast (matches the pre-collapse worker) and clear.
            Ok((n, RotateOutcome::GuardBusy)) => {
                let _ = sender.send(OpResult {
                    name: n.clone(),
                    kind: ActivityKind::Refreshing,
                    outcome: Err(anyhow::anyhow!("failed to acquire rotation lock")),
                });
                clear_activity(activity, &n);
            }
            // Persist/skip legs already emitted their OpResult and cleared their
            // slot; a re-clear is idempotent and guards the skipped-no-token path.
            Ok((n, RotateOutcome::Persisted(false))) => clear_activity(activity, &n),
            Err(_) => {
                // Worker panicked before `clear_activity`. Clear here so the
                // spinner doesn't freeze and `any_busy` can resolve. No OpResult
                // was sent, so no toast for this profile.
                clear_activity(activity, &name);
            }
        }
    }
    if let Ok(mut q) = refetch.lock() {
        for name in &refreshed {
            q.insert(name.clone());
        }
    }
    refreshed
}

/// Names of opted-in OAuth profiles with NO live 5-hour window eligible for an
/// auto-start kick: not active-with-a-diverged-link and past the cooldown.
/// Caller enqueues into `pending_auto_start`; the on-tick drain kicks each via
/// [`start_window`].
///
/// The single steady-state arming path, and the fix for background (non-active)
/// accounts: the active profile gets its window for free (CC opens one on use),
/// so without a recurring scan a background profile whose startup kick failed,
/// or that lost its window mid-session, would never be re-armed. Running every
/// tick re-arms them as soon as they fall idle and windowless.
///
/// A live `clauth start` session is NOT a skip reason: CC holds the lock but
/// only opens a window on first message, so an idle/reset session has a live
/// lock and no window — exactly what needs arming. `has_active_window` already
/// excludes sessions that hold a window; liveness is the wrong signal. Kicking
/// spends only the access token (never the single-use refresh token), so it
/// can't race the chain — unlike the rotate paths, which gate on
/// `has_live_session`.
///
/// Reads the usage store BEFORE the config lock: `apply_usage` holds
/// `usage_store` then `config`, so the reverse order here inverts the global
/// rank and deadlocks.
pub(crate) fn windowless_auto_start_candidates(
    config: &crate::profile::ConfigHandle,
    store: &UsageStore,
) -> Vec<String> {
    // A window is live only while `resets_at` is still in the future. Checking
    // mere presence would treat an expired-but-not-yet-cleared `resets_at` as
    // live, so a background profile whose window lapsed would never be re-armed
    // (the active profile gets a fresh window from CC each session, masking the
    // bug as "auto-start only works for the active one").
    let now_secs = now_epoch_secs();
    let has_active_window: std::collections::HashMap<String, bool> = store
        .lock()
        .ok()
        .map(|s| {
            s.iter()
                .map(|(name, info)| {
                    let active = info
                        .five_hour
                        .as_ref()
                        .and_then(|w| w.resets_at.as_deref())
                        .and_then(iso_to_epoch_secs)
                        .is_some_and(|resets_at| now_secs < resets_at);
                    (name.clone(), active)
                })
                .collect()
        })
        .unwrap_or_default();

    let cfg = config.lock().expect("config mutex poisoned");
    let skip_active = active_link_diverged(&cfg);
    let now = now_ms();
    cfg.profiles
        .iter()
        .filter(|p| {
            p.auto_start
                && p.is_oauth()
                && !(skip_active && cfg.is_active(&p.name))
                && !*has_active_window.get(p.name.as_str()).unwrap_or(&false)
                && now.saturating_sub(
                    cfg.state
                        .last_auto_start_at
                        .get(p.name.as_str())
                        .copied()
                        .unwrap_or(0),
                ) >= AUTO_START_COOLDOWN_MS
        })
        .map(|p| p.name.to_string())
        .collect()
}

/// The single auto-start codepath: fire the 1-token Haiku ping that opens a
/// profile's 5-hour window using its CURRENT access token. NEVER refreshes the
/// OAuth chain — the fetch path's 401-rotation keeps the access token valid, so
/// auto-start can't double-spend the single-use refresh token and needs no
/// `RotationGuard`.
///
/// Gates on opt-in, OAuth, diverged active link, and cooldown so every caller
/// shares one rule set. Does NOT gate on `has_live_session`: a kick spends only
/// the access token, and an idle/reset session with no window is precisely what
/// needs arming — see [`windowless_auto_start_candidates`]. The cooldown slot
/// (`last_auto_start_at`) is stamped before the kick as a concurrency guard so a
/// second tick can't spawn a duplicate worker in the gap between the idle check
/// and the worker starting. On success the stamp stays (full 4.5 h cooldown); on
/// failure it's overwritten with a backoff allowing a retry after
/// `AUTO_START_RETRY_MS` (5 min) so a transient error / just-rotated token
/// recovers without waiting for the next window. The "already has a window"
/// check lives in [`windowless_auto_start_candidates`], where the store is on
/// hand.
///
/// Marks `AutoStarting`, sends one `OpResult`, and on success pushes `name` onto
/// `refetch` so usage re-fetches and the armed window shows up. Returns true iff
/// the kick succeeded.
///
/// `refetch`/`activity` are optional scheduler side-channels: the TUI passes
/// `Some` to re-fetch and drive the spinner; the no-scheduler CLI switch passes
/// `None` (no queue drain, no spinner), allocating no throwaway mutexes.
pub(crate) fn start_window(
    config: &crate::profile::ConfigHandle,
    name: &str,
    refetch: Option<&RefetchQueue>,
    activity: Option<&ActivityStore>,
    sender: &OpResultSender,
) -> bool {
    let access_token = {
        let mut cfg = config.lock().expect("config mutex poisoned");
        match with_state_lock(|| {
            let Some(profile) = cfg.find(name) else {
                return Ok::<_, anyhow::Error>(None);
            };
            if !profile.is_oauth() || !profile.auto_start {
                return Ok(None);
            }
            let Some(token) = profile.access_token().map(str::to_string) else {
                return Ok(None);
            };
            if active_link_diverged(&cfg) && cfg.is_active(name) {
                return Ok(None);
            }
            let now = now_ms();
            let last = cfg.state.last_auto_start_at.get(name).copied().unwrap_or(0);
            if now.saturating_sub(last) < AUTO_START_COOLDOWN_MS {
                return Ok(None);
            }
            cfg.state.last_auto_start_at.insert(name.to_string(), now);
            let _ = save_app_state(&cfg.state);
            Ok(Some(token))
        }) {
            Ok(Some(t)) => t,
            _ => return false,
        }
    };

    if let Some(activity) = activity {
        mark_activity(activity, name, ProfileActivity::AutoStarting);
    }
    let outcome = kick(&access_token);
    let kicked = outcome.is_ok();
    // On failure: overwrite the pre-kick stamp with a backoff so the profile
    // retries after AUTO_START_RETRY_MS instead of the full 4.5 h.
    if !kicked {
        let mut cfg = config.lock().expect("config mutex poisoned");
        let _ = with_state_lock(|| {
            let backoff =
                now_ms().saturating_sub(AUTO_START_COOLDOWN_MS.saturating_sub(AUTO_START_RETRY_MS));
            cfg.state
                .last_auto_start_at
                .insert(name.to_string(), backoff);
            let _ = save_app_state(&cfg.state);
            Ok::<_, anyhow::Error>(())
        });
    }
    if let Some(activity) = activity {
        clear_activity(activity, name);
    }
    let _ = sender.send(OpResult {
        name: name.to_string(),
        kind: ActivityKind::AutoStarting,
        outcome,
    });
    if kicked
        && let Some(refetch) = refetch
        && let Ok(mut q) = refetch.lock()
    {
        q.insert(name.to_string());
    }
    kicked
}

/// Write rotated token fields into an OAuth block. Caller holds the state lock.
fn write_token_fields(oauth: &mut OAuthToken, tok: TokenResponse) {
    oauth.access_token = tok.access_token;
    oauth.refresh_token = Some(tok.refresh_token);
    oauth.expires_at = Some((now_ms() + tok.expires_in * 1000) as i64);
    if let Some(scope) = tok.scope {
        oauth.scopes = Some(scope.split_whitespace().map(String::from).collect());
    }
}

/// Write a rotated token pair into the named profile's OAuth block and persist.
/// Takes `&ConfigHandle` so workers can call from a thread without holding the
/// lock across HTTP. Returns `Ok(())` so callers `?` straight into their
/// OpResult. Errs (never silently no-ops) when the profile/OAuth block is
/// missing, the save fails, or the state flock can't be taken — callers must
/// refuse to act on the rotated pair in every case. Every persist-side failure
/// uses the same "failed to persist rotated tokens" message so the toast text is
/// identical regardless of leg (none reachable in practice — a profile selected
/// for rotation always has an OAuth block).
pub(crate) fn apply_rotated_tokens_locked(
    config: &crate::profile::ConfigHandle,
    name: &str,
    tok: TokenResponse,
) -> Result<()> {
    let mut cfg = config.lock().expect("config mutex poisoned");
    with_state_lock(|| {
        let Some(profile) = cfg.find_mut(name) else {
            return Err(anyhow::anyhow!("failed to persist rotated tokens"));
        };
        let Some(creds) = profile.credentials.as_mut() else {
            return Err(anyhow::anyhow!("failed to persist rotated tokens"));
        };
        let Some(oauth) = creds.claude_ai_oauth.as_mut() else {
            return Err(anyhow::anyhow!("failed to persist rotated tokens"));
        };
        write_token_fields(oauth, tok);
        // Stage the rotated pair durably before the structured save (see
        // `stage_rotated_credentials`): a failed save or crash is recovered on
        // next load rather than stranding a dead single-use refresh chain.
        if let Some(creds) = profile.credentials.as_ref() {
            let _ = stage_rotated_credentials(name, creds);
        }
        if save_profile(profile).is_err() {
            // Sidecar stays in place; load_profile adopts it on the next start.
            return Err(anyhow::anyhow!("failed to persist rotated tokens"));
        }
        clear_staged_credentials(name);
        Ok(())
    })
    // A failed state flock surfaces as the `Err` from `with_state_lock`, so a
    // poisoned/unavailable lock never looks like a successful rotation.
}

/// True when an active profile is set and its live .credentials.json no longer
/// resolves to that profile's stored credentials. Then the in-memory tokens are
/// stale relative to what CC just wrote, so rotating them would leak a refresh
/// chain nobody will use.
fn active_link_diverged(config: &AppConfig) -> bool {
    config.state.active_profile.as_deref().is_some_and(|name| {
        matches!(
            classify_credentials_link(name).ok(),
            Some(LinkState::Diverged)
        )
    })
}

#[cfg(test)]
#[path = "../tests/inline/oauth.rs"]
mod tests;
