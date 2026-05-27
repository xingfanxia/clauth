use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::claude::{LinkState, classify_credentials_link};
use crate::lock::with_state_lock;
use crate::profile::{AppConfig, OAuthToken, save_app_state, save_profile};
use crate::runtime::has_live_session;
use crate::usage::{
    ActivityKind, ActivityStore, LastRotatedWindow, OpResult, OpResultSender, ProfileActivity,
    RefetchQueue, UsageStore, clear_activity, mark_activity, now_ms,
};

/// Anthropic's OAuth token endpoint. Same one Claude Code uses on startup
/// to mint a fresh access token from the stored refresh token.
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

/// Refuse to re-ping a profile until this long after its last auto-start.
/// Sized just under the 5-hour window so a ping whose follow-up usage fetch
/// failed (network blip, stale endpoint) doesn't cause us to re-fire on
/// every refresh, while still allowing a fresh ping once the window has
/// elapsed.
const AUTO_START_COOLDOWN_MS: u64 = 4 * 3600 * 1000 + 30 * 60 * 1000;

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

/// Sends a 1-token Haiku message to start the 5-hour usage window. Mirrors
/// what Claude Code does silently on launch.
fn kick(access_token: &str) -> Result<()> {
    let body = serde_json::to_string(&serde_json::json!({
        "model": KICK_MODEL,
        "max_tokens": 1,
        "system": [{ "type": "text", "text": KICK_SYSTEM_PROMPT }],
        "messages": [{ "role": "user", "content": "x" }],
    }))?;

    AGENT
        .post(MESSAGES_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .send(&body)
        .map_err(crate::ureq_error::into_anyhow)?;
    Ok(())
}

/// Rotate the OAuth token chain for a single named profile. Returns true iff
/// the new pair was persisted. Skips when the profile has no refresh token or
/// a live `clauth start` session holds the chain (same gate as `refresh_all`).
///
/// No cooldown gating — the caller is responsible for deduplication via
/// `LastRotatedWindow`. Does not touch `last_auto_start_at`.
///
/// Takes `Arc<Mutex<AppConfig>>` so the lock is held only across the brief
/// read/write windows around HTTP, not across the network round trip.
/// Emits one `OpResult { kind: Refreshing }` on the supplied sender unless
/// the profile is skipped (no refresh token / live session).
pub(crate) fn rotate_one(
    config: &Arc<Mutex<AppConfig>>,
    name: &str,
    activity: &ActivityStore,
    sender: &OpResultSender,
) -> bool {
    let token = {
        let cfg = config.lock().expect("config mutex poisoned");
        with_state_lock(|| {
            if has_live_session(name) {
                return Ok::<_, anyhow::Error>(None);
            }
            let rt = cfg
                .find(name)
                .and_then(|p| p.refresh_token().map(str::to_string));
            if rt.is_some() {
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
        return false;
    };
    let refreshed = refresh(&rt);
    let (outcome, applied) = match refreshed {
        Ok(tok) => {
            let saved = apply_rotated_tokens_locked(config, name, tok, None);
            if saved {
                (Ok(()), true)
            } else {
                (
                    Err(anyhow::anyhow!("failed to persist rotated tokens")),
                    false,
                )
            }
        }
        Err(e) => (Err(e), false),
    };
    clear_activity(activity, name);
    let _ = sender.send(OpResult {
        name: name.to_string(),
        kind: ActivityKind::Refreshing,
        outcome,
    });
    applied
}

/// Window-expiry variant of [`rotate_one`] that stamps `LastRotatedWindow`
/// atomically with the credential write. Use this from the on_tick
/// window-expiry dispatcher instead of `rotate_one + manual LRW insert`.
///
/// The stamp happens inside `apply_rotated_tokens_locked` under the same
/// state-lock acquisition as the credential write, so no panic or
/// mutex-poison between persist and stamp can cause the scheduler to
/// re-enqueue and burn an already-rotated refresh token chain.
///
/// Returns true iff the rotation was persisted (same as `rotate_one`).
/// On `has_live_session = true` the profile is skipped and LRW is
/// left untouched — `scan_expired_windows` will re-enqueue next tick
/// (no HTTP, benign).
pub(crate) fn rotate_one_for_window(
    config: &Arc<Mutex<AppConfig>>,
    name: &str,
    activity: &ActivityStore,
    sender: &OpResultSender,
    lrw: &LastRotatedWindow,
    resets_at: i64,
) -> bool {
    let token = {
        let cfg = config.lock().expect("config mutex poisoned");
        with_state_lock(|| {
            if has_live_session(name) {
                return Ok::<_, anyhow::Error>(None);
            }
            let rt = cfg
                .find(name)
                .and_then(|p| p.refresh_token().map(str::to_string));
            if rt.is_some() {
                mark_activity(activity, name, ProfileActivity::Refreshing);
            }
            Ok(rt)
        })
        .ok()
        .flatten()
    };

    let Some(rt) = token else {
        return false;
    };
    let refreshed = refresh(&rt);
    let (outcome, applied) = match refreshed {
        Ok(tok) => {
            let saved = apply_rotated_tokens_locked(config, name, tok, Some((lrw, resets_at)));
            if saved {
                (Ok(()), true)
            } else {
                (
                    Err(anyhow::anyhow!("failed to persist rotated tokens")),
                    false,
                )
            }
        }
        Err(e) => (Err(e), false),
    };
    clear_activity(activity, name);
    let _ = sender.send(OpResult {
        name: name.to_string(),
        kind: ActivityKind::Refreshing,
        outcome,
    });
    applied
}

/// Profiles that would be rotated by `refresh_all`. Extracted so tests can
/// pin the inclusion logic without touching the network.
///
/// Returns `(name, refresh_token)` pairs. Diverged-active is skipped unless
/// `force` is true; live-session profiles are included only when `force` is true.
pub(crate) fn rotation_candidates(config: &AppConfig, force: bool) -> Vec<(String, String)> {
    // when force=true (t-key rotate-all) we bypass diverged-active: the user
    // explicitly wants every account rotated, including the one CC is touching.
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
            Some((p.name.clone(), p.refresh_token()?.to_string()))
        })
        .collect()
}

/// Refreshes every profile's OAuth token pair (rotated pair saved to disk).
/// Mirrors what Claude Code does silently on launch — minus the kick.
///
/// Profiles without a stored refresh token are skipped. Network or revocation
/// failures are swallowed per-profile; cached state stays put for those.
///
/// When `force` is true both the `has_live_session` guard and the diverged-active
/// guard are bypassed — the user explicitly requested every profile be rotated.
///
/// Returns the names of profiles whose token rotation succeeded so the caller
/// can target follow-up work (usage re-fetch, kick) at the same set.
/// Pushes each rotated name onto `refetch` so the next scheduler tick
/// re-fetches usage immediately without waiting for the cadence.
///
/// Takes `&Arc<Mutex<AppConfig>>` so per-profile workers can lock/unlock
/// independently around their HTTP calls without ever holding the config
/// mutex across the network. Each per-profile worker emits one `OpResult`
/// on `sender` the moment its HTTP completes, so the spinner clears in
/// arrival order rather than waiting for the slowest sibling.
pub(crate) fn refresh_all(
    config: &Arc<Mutex<AppConfig>>,
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

    // Pair each handle with the profile name so the join loop can clear the
    // activity slot on panic — the name is consumed by the closure, so we
    // need a second copy held outside it.
    let handles: Vec<(String, _)> = snapshots
        .into_iter()
        .map(|(name, rt)| {
            let config = Arc::clone(config);
            let activity = Arc::clone(activity);
            let sender = sender.clone();
            let name_for_handle = name.clone();
            let h = std::thread::spawn(move || {
                let refreshed = refresh(&rt);
                let (outcome, saved) = match refreshed {
                    Ok(tok) => {
                        let ok = apply_rotated_tokens_locked(&config, &name, tok, None);
                        if ok {
                            (Ok(()), true)
                        } else {
                            (
                                Err(anyhow::anyhow!("failed to persist rotated tokens")),
                                false,
                            )
                        }
                    }
                    Err(e) => (Err(e), false),
                };
                clear_activity(&activity, &name);
                let _ = sender.send(OpResult {
                    name: name.clone(),
                    kind: ActivityKind::Refreshing,
                    outcome,
                });
                (name, saved)
            });
            (name_for_handle, h)
        })
        .collect();

    let mut refreshed = Vec::new();
    for (name, h) in handles {
        match h.join() {
            Ok((n, true)) => refreshed.push(n),
            Ok(_) => {}
            Err(_) => {
                // Worker panicked before calling `clear_activity`. Clear the slot
                // here so the spinner doesn't freeze and `any_busy` can resolve.
                // No OpResult was sent, so no toast is emitted for this profile.
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

/// For every profile that opted in via `auto_start = true` and currently has
/// no 5-hour usage window, refreshes its OAuth tokens (rotated pair saved to
/// disk) and fires a 1-token Haiku ping to start the window.
///
/// Returns the names of profiles whose ping succeeded so the caller can
/// re-fetch usage and confirm the window now shows up. Pushes each kicked
/// name onto `refetch` so the scheduler re-fetches without waiting for the
/// cadence.
pub(crate) fn auto_start_windows(
    config: &Arc<Mutex<AppConfig>>,
    store: &UsageStore,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
    sender: &OpResultSender,
) -> Vec<String> {
    // Snapshot which profiles already have an active 5h window BEFORE taking
    // the config lock. `apply_usage` on the UI thread holds `usage_store` then
    // acquires `config`; taking `config` first and then `store` inside
    // `with_state_lock` would invert that order and deadlock. Reading into an
    // owned map here keeps the lock order consistent: store snapshot first,
    // config lock second.
    let has_active_window: std::collections::HashMap<String, bool> = store
        .lock()
        .ok()
        .map(|s| {
            s.iter()
                .map(|(name, info)| {
                    let active = info
                        .five_hour
                        .as_ref()
                        .and_then(|w| w.resets_at.as_ref())
                        .is_some();
                    (name.clone(), active)
                })
                .collect()
        })
        .unwrap_or_default();

    // Claim cooldown slots under the lock BEFORE any network work. A competing
    // clauth process that starts up a moment later will observe our recorded
    // `last_auto_start_at` and skip the same profile, so the refresh token
    // rotates exactly once even when two instances race startup.
    //
    // Holding the lock during the OAuth/messages HTTP round trips would stall
    // every other instance for seconds, so we release between claim and work.
    let snapshots: Vec<(String, String)> = {
        let mut cfg = config.lock().expect("config mutex poisoned");
        match with_state_lock(|| {
            let skip_active = active_link_diverged(&cfg);
            let now = now_ms();
            let mut claimed = Vec::new();
            for profile in &cfg.profiles {
                if !profile.auto_start {
                    continue;
                }
                if skip_active && cfg.is_active(&profile.name) {
                    continue;
                }
                if has_live_session(&profile.name) {
                    continue;
                }
                if *has_active_window.get(&profile.name).unwrap_or(&false) {
                    continue;
                }
                let last = cfg
                    .state
                    .last_auto_start_at
                    .get(&profile.name)
                    .copied()
                    .unwrap_or(0);
                if now.saturating_sub(last) < AUTO_START_COOLDOWN_MS {
                    continue;
                }
                let Some(token) = profile.refresh_token().map(str::to_string) else {
                    continue;
                };
                claimed.push((profile.name.clone(), token));
            }

            // Claim cooldown slots before releasing the lock. A competing
            // clauth process will observe these timestamps and skip the same
            // profiles so the token rotates exactly once even when two
            // instances race startup.
            for (name, _) in &claimed {
                cfg.state.last_auto_start_at.insert(name.clone(), now);
            }
            if !claimed.is_empty() {
                let _ = save_app_state(&cfg.state);
            }
            Ok::<_, anyhow::Error>(claimed)
        }) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        }
    };

    // Refresh and kick are reported separately. Anthropic rotates the refresh
    // token on every successful refresh, so the new pair MUST be persisted
    // before the kick is attempted — otherwise a kick-only failure leaves the
    // stored refresh token permanently invalid.
    //
    // Spinner-wise: AutoStarting covers refresh + persist + kick for the full
    // window. Each worker stamps its own slot AutoStarting before the network
    // round trip, posts an OpResult on completion, and clears its slot so
    // the spinner drops at HTTP completion rather than waiting for the
    // slowest sibling.
    for (name, _) in &snapshots {
        mark_activity(activity, name, ProfileActivity::AutoStarting);
    }
    // Pair each handle with the profile name so the join loop can clear the
    // activity slot on panic — the name is consumed by the closure.
    let handles: Vec<(String, _)> = snapshots
        .into_iter()
        .map(|(name, rt)| {
            let config = Arc::clone(config);
            let activity = Arc::clone(activity);
            let sender = sender.clone();
            let name_for_handle = name.clone();
            let h = std::thread::spawn(move || {
                let (outcome, kicked) = run_auto_start(&config, &name, &rt);
                clear_activity(&activity, &name);
                let _ = sender.send(OpResult {
                    name: name.clone(),
                    kind: ActivityKind::AutoStarting,
                    outcome,
                });
                (name, kicked)
            });
            (name_for_handle, h)
        })
        .collect();

    let mut kicked = Vec::new();
    for (name, h) in handles {
        match h.join() {
            Ok((n, true)) => kicked.push(n),
            Ok(_) => {}
            Err(_) => {
                // Worker panicked before calling `clear_activity`. Clear the slot
                // here so the spinner doesn't freeze and `any_busy` can resolve.
                clear_activity(activity, &name);
            }
        }
    }
    if let Ok(mut q) = refetch.lock() {
        for name in &kicked {
            q.insert(name.clone());
        }
    }
    kicked
}

/// Per-profile auto-start work: refresh tokens, persist, kick the 5h window.
/// Returns `(outcome, kicked)` where `kicked` is true iff the messages POST
/// succeeded. Used by both `auto_start_windows` (fan-out) and the single-name
/// `auto_start_named` path. Never holds the config mutex across HTTP.
fn run_auto_start(
    config: &Arc<Mutex<AppConfig>>,
    name: &str,
    refresh_token: &str,
) -> (Result<()>, bool) {
    let tok = match refresh(refresh_token) {
        Ok(t) => t,
        Err(e) => return (Err(e), false),
    };
    let access_token = tok.access_token.clone();
    if !apply_rotated_tokens_or_rollback_cooldown_locked(config, name, tok) {
        return (
            Err(anyhow::anyhow!("failed to persist rotated tokens")),
            false,
        );
    }
    match kick(&access_token) {
        Ok(()) => (Ok(()), true),
        Err(e) => (Err(e), false),
    }
}

/// Refresh tokens and kick the 5h window for one named profile. Same
/// cooldown + persistence semantics as `auto_start_windows`, just scoped to
/// a single name and without a usage-store check — callers use this where
/// no fresh `UsageStore` is on hand (e.g. one-shot CLI switch). The 4.5h
/// cooldown stops repeated invocations from re-firing inside an open
/// window. Returns true iff the kick HTTP call succeeded. Pushes `name`
/// onto `refetch` on success.
pub(crate) fn auto_start_named(
    config: &Arc<Mutex<AppConfig>>,
    name: &str,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
    sender: &OpResultSender,
) -> bool {
    let now = now_ms();
    let token = {
        let mut cfg = config.lock().expect("config mutex poisoned");
        match with_state_lock(|| {
            let Some(profile) = cfg.find(name) else {
                return Ok::<_, anyhow::Error>(None);
            };
            if !profile.auto_start {
                return Ok(None);
            }
            if active_link_diverged(&cfg) && cfg.is_active(name) {
                return Ok(None);
            }
            if has_live_session(name) {
                return Ok(None);
            }
            let last = cfg.state.last_auto_start_at.get(name).copied().unwrap_or(0);
            if now.saturating_sub(last) < AUTO_START_COOLDOWN_MS {
                return Ok(None);
            }
            let Some(rt) = profile.refresh_token().map(str::to_string) else {
                return Ok(None);
            };
            cfg.state.last_auto_start_at.insert(name.to_string(), now);
            let _ = save_app_state(&cfg.state);
            Ok(Some(rt))
        }) {
            Ok(Some(t)) => t,
            _ => return false,
        }
    };

    // AutoStarting spinner covers the full refresh + persist + kick window.
    // Never held across `with_state_lock`; cleared in every exit path.
    mark_activity(activity, name, ProfileActivity::AutoStarting);
    let (outcome, kicked) = run_auto_start(config, name, &token);
    clear_activity(activity, name);
    let _ = sender.send(OpResult {
        name: name.to_string(),
        kind: ActivityKind::AutoStarting,
        outcome,
    });
    if kicked && let Ok(mut q) = refetch.lock() {
        q.insert(name.to_string());
    }
    kicked
}

/// Write rotated token fields into an OAuth block. Called under the state lock.
fn write_token_fields(oauth: &mut OAuthToken, tok: TokenResponse) {
    oauth.access_token = tok.access_token;
    oauth.refresh_token = Some(tok.refresh_token);
    oauth.expires_at = Some((now_ms() + tok.expires_in * 1000) as i64);
    if let Some(scope) = tok.scope {
        oauth.scopes = Some(scope.split_whitespace().map(String::from).collect());
    }
}

/// Write a rotated token pair into the named profile's OAuth block and persist.
/// If persist fails, rolls back the `last_auto_start_at` cooldown so the next
/// run can retry without waiting the full 4.5h. Returns true on success.
///
/// Takes `&Arc<Mutex<AppConfig>>` and locks it only across the brief write
/// window, so workers can call this without holding the lock during HTTP.
///
/// Lock order: AppConfig in-process mutex first, then state flock. Matches
/// the existing UI-thread order so workers and the UI thread never invert.
fn apply_rotated_tokens_or_rollback_cooldown_locked(
    config: &Arc<Mutex<AppConfig>>,
    name: &str,
    tok: TokenResponse,
) -> bool {
    let mut cfg = config.lock().expect("config mutex poisoned");
    with_state_lock(|| {
        let Some(profile) = cfg.find_mut(name) else {
            return Ok::<_, anyhow::Error>(false);
        };
        let Some(creds) = profile.credentials.as_mut() else {
            return Ok(false);
        };
        let Some(oauth) = creds.claude_ai_oauth.as_mut() else {
            return Ok(false);
        };
        write_token_fields(oauth, tok);
        if save_profile(profile).is_err() {
            // Roll back the cooldown so the profile isn't stranded.
            cfg.state.last_auto_start_at.remove(name);
            let _ = save_app_state(&cfg.state);
            return Ok(false);
        }
        Ok(true)
    })
    .unwrap_or(false)
}

/// Write a rotated token pair into the named profile's OAuth block and
/// persist. Returns true on success. No-op when the profile or OAuth block
/// is missing — callers that care can refuse to act on `false`.
///
/// When `window_stamp` is `Some((lrw, resets_at))`, the `LastRotatedWindow`
/// map is updated atomically with the credential write — under the same
/// state-lock acquisition — so no panic or mutex-poison between the persist
/// and the stamp can cause a silent chain burn on the next scheduler tick.
/// Lock order: AppConfig → state flock → LRW leaf mutex.
///
/// Locking variant of [`apply_rotated_tokens`]: takes `&Arc<Mutex<AppConfig>>`
/// so workers can call from a thread without holding the lock across HTTP.
pub(crate) fn apply_rotated_tokens_locked(
    config: &Arc<Mutex<AppConfig>>,
    name: &str,
    tok: TokenResponse,
    window_stamp: Option<(&LastRotatedWindow, i64)>,
) -> bool {
    let mut cfg = config.lock().expect("config mutex poisoned");
    with_state_lock(|| {
        let Some(profile) = cfg.find_mut(name) else {
            return Ok::<_, anyhow::Error>(false);
        };
        let Some(creds) = profile.credentials.as_mut() else {
            return Ok(false);
        };
        let Some(oauth) = creds.claude_ai_oauth.as_mut() else {
            return Ok(false);
        };
        write_token_fields(oauth, tok);
        if save_profile(profile).is_err() {
            return Ok(false);
        }
        if let Some((lrw, resets_at)) = window_stamp
            && let Ok(mut guard) = lrw.lock()
        {
            guard.insert(name.to_string(), resets_at);
        }
        Ok(true)
    })
    .unwrap_or(false)
}

/// Write a rotated token pair into the named profile's OAuth block and
/// persist. Returns true on success. No-op when the profile or OAuth block
/// is missing — callers that care can refuse to act on `false`.
///
/// `&mut AppConfig` variant for callers that already hold the lock (e.g. the
/// divergence-probe path on the UI thread).
pub(crate) fn apply_rotated_tokens(config: &mut AppConfig, name: &str, tok: TokenResponse) -> bool {
    with_state_lock(|| {
        let Some(profile) = config.find_mut(name) else {
            return Ok::<_, anyhow::Error>(false);
        };
        let Some(creds) = profile.credentials.as_mut() else {
            return Ok(false);
        };
        let Some(oauth) = creds.claude_ai_oauth.as_mut() else {
            return Ok(false);
        };
        write_token_fields(oauth, tok);
        Ok(save_profile(profile).is_ok())
    })
    .unwrap_or(false)
}

/// True when an active profile is set and its live .credentials.json no
/// longer resolves to that profile's stored credentials. In that state, the
/// in-memory tokens are stale relative to whatever CC just wrote, so
/// rotating them would leak a refresh chain nobody will use.
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
