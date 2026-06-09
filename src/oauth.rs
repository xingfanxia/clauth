use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::claude::{LinkState, classify_credentials_link};
use crate::lock::with_state_lock;
use crate::profile::{
    AppConfig, OAuthToken, clear_staged_credentials, save_profile, stage_rotated_credentials,
};
use crate::runtime::{RotationGuard, has_live_session};
use crate::usage::{
    ActivityStore, OpResult, OpResultSender, ProfileActivity, RefetchQueue, clear_activity,
    mark_activity, now_ms,
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

/// Pause between the steps of the 401/429-recovery sequence (failed kick →
/// rotate → retry kick → usage re-fetch) so the API sees the rotated pair settle
/// instead of three back-to-back requests on the same chain.
const ROTATION_STEP_DELAY_MS: u64 = 2000;

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
        // ureq 3 defaults non-2xx to `Err(Error::StatusCode)`, which `kick`'s
        // error mapping collapsed into `KickError::Other` — making the
        // 401 → rotate-and-retry leg unreachable. With the flag off, `kick`
        // reads the status from the `Ok` response and `refresh` checks it
        // explicitly below.
        .http_status_as_error(false)
        .build()
        .into()
});

pub(crate) fn refresh(refresh_token: &str) -> Result<TokenResponse> {
    let body = serde_json::to_string(&serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    }))?;

    let mut response = AGENT
        .post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .send(&body)
        .map_err(crate::ureq_error::into_anyhow)?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(crate::ureq_error::into_anyhow)?;
    if status >= 400 {
        anyhow::bail!("HTTP {status}: {text}");
    }

    serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{e}: {text}"))
}

/// A kick failure. Distinguishes a 401 (access token expired — rotate the chain
/// and retry) from every other failure (body encode, transport, or any non-401
/// HTTP status), which is terminal for this attempt. Mirrors `FetchError::Status`
/// so the auto-start rotation leg reacts to the same signal the fetch path does.
enum KickError {
    /// The Messages endpoint returned this >=400 status.
    Status(u16),
    /// Body encode or transport failure before a status was seen.
    Other(anyhow::Error),
}

impl From<KickError> for anyhow::Error {
    fn from(e: KickError) -> Self {
        match e {
            KickError::Status(s) => anyhow::anyhow!("HTTP {s}"),
            KickError::Other(e) => e,
        }
    }
}

/// Sends a 1-token Haiku message to start the 5-hour usage window. Mirrors what
/// Claude Code does silently on launch.
fn kick(access_token: &str) -> std::result::Result<(), KickError> {
    let body = serde_json::to_string(&serde_json::json!({
        "model": KICK_MODEL,
        "max_tokens": 1,
        "system": [{ "type": "text", "text": KICK_SYSTEM_PROMPT }],
        "messages": [{ "role": "user", "content": "x" }],
    }))
    .map_err(|e| KickError::Other(e.into()))?;

    let status = AGENT
        .post(MESSAGES_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .send(&body)
        .map_err(|e| KickError::Other(crate::ureq_error::into_anyhow(e)))?
        .status()
        .as_u16();
    if status >= 400 {
        return Err(KickError::Status(status));
    }
    Ok(())
}

/// Outcome of an [`auto_start_kick`]. `opened` is whether the 5h window opened
/// (a 2xx from the messages endpoint, first try or post-rotation retry).
/// `rotated` carries a freshly minted `(access, refresh)` pair whenever a
/// rotation happened — the caller MUST propagate it into the live token snapshot
/// even when `opened` is false, because the single-use refresh token was already
/// spent.
pub(crate) struct KickResult {
    pub(crate) opened: bool,
    pub(crate) rotated: Option<(String, Option<String>)>,
}

impl KickResult {
    fn not_opened() -> Self {
        Self {
            opened: false,
            rotated: None,
        }
    }
}

/// Fire the 1-token Haiku ping that opens a profile's 5h window. On a 401
/// (expired access token) it rotates the chain once and retries. On a 429
/// (rate-limited) it rotates ONLY when `access_expires_at` is in the past — a
/// clock-expired token is the one case where a refresh could actually unstick
/// the kick. A 429 on a still-valid token is a pure endpoint rate limit a
/// refresh can't fix; rotating it would spend the single-use refresh token every
/// 60s tick under a sustained 429 (the steady-state fetch path refuses 429
/// rotation entirely for exactly this reason). Unknown expiry (`None`) is
/// treated as not-expired, so it does not rotate.
///
/// Same double-spend guards as `fetch_with_rotation`'s rotation leg:
/// `RotationGuard` outermost across the refresh HTTP window, a `has_live_session`
/// re-check under the guard (a live session refreshes the chain itself), and the
/// rotated pair returned to the caller for the live token snapshot. A first kick
/// that succeeds spends only the access token and takes no `RotationGuard`.
///
/// Each recovery step is paced by [`ROTATION_STEP_DELAY_MS`] (kick → rotate →
/// retry kick → caller's usage re-fetch); none of the sleeps holds the rotation
/// lock. `activity` (the scheduler's store) drives the spinner; the CLI passes
/// `None`.
pub(crate) fn auto_start_kick(
    config: &crate::profile::ConfigHandle,
    name: &str,
    access_token: &str,
    refresh_token: Option<&str>,
    access_expires_at: Option<i64>,
    activity: Option<&ActivityStore>,
) -> KickResult {
    match kick(access_token) {
        Ok(()) => {
            return KickResult {
                opened: true,
                rotated: None,
            };
        }
        // Expired token (401): always rotate once and retry.
        Err(KickError::Status(401)) => {}
        // Rate limit (429): rotate only if the access token is also clock-expired;
        // a still-valid token can't be unstuck by a refresh, so refuse to spend it.
        Err(KickError::Status(429))
            if access_expires_at.is_some_and(|exp| now_ms() as i64 >= exp) => {}
        Err(_) => return KickResult::not_opened(),
    }

    let Some(rt) = refresh_token else {
        return KickResult::not_opened();
    };
    // Pace the recovery before any lock is taken.
    std::thread::sleep(std::time::Duration::from_millis(ROTATION_STEP_DELAY_MS));
    // RotationGuard outermost across the HTTP window — acquired with no other
    // lock held (the caller released the usage store before kicking).
    let Ok(rotation_guard) = RotationGuard::acquire(name) else {
        return KickResult::not_opened();
    };
    if has_live_session(name) {
        return KickResult::not_opened();
    }

    // Refresh spinner during the round trip, then back to Fetching for the retry
    // kick + the caller's fetch (the kick runs inside the scheduler's fetch leg).
    if let Some(activity) = activity {
        mark_activity(activity, name, ProfileActivity::Refreshing);
    }
    let refreshed = refresh(rt);
    if let Some(activity) = activity {
        mark_activity(activity, name, ProfileActivity::Fetching);
    }
    let tok = match refreshed {
        Ok(t) => t,
        Err(_) => return KickResult::not_opened(),
    };

    let access = tok.access_token.clone();
    let new_refresh = tok.refresh_token.clone();
    if apply_rotated_tokens_locked(config, name, tok).is_err() {
        return KickResult::not_opened();
    }
    // The pair is persisted; carry it back so the caller syncs the live snapshot
    // even if the retry kick below fails — the refresh token was spent either way.
    let rotated = Some((access.clone(), Some(new_refresh)));
    // Retry kick spends only the access token, so release the rotation lock
    // before the paced waits — a sibling worker shouldn't block on our sleeps.
    drop(rotation_guard);

    // Pace rotate → retry kick, then retry kick → the caller's usage re-fetch.
    std::thread::sleep(std::time::Duration::from_millis(ROTATION_STEP_DELAY_MS));
    let opened = kick(&access).is_ok();
    std::thread::sleep(std::time::Duration::from_millis(ROTATION_STEP_DELAY_MS));
    KickResult { opened, rotated }
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

/// Force-rotate a single profile's OAuth token pair — one [`refresh_all`] worker
/// leg, scoped to `name` (the action-menu "rotate tokens" on the focused account).
/// Same discipline: `rotate_one_inner` holds the per-profile RotationGuard across
/// the HTTP window with a `has_live_session` re-check, so the single-use refresh
/// token can't double-spend. On success the profile is pushed onto `refetch` so
/// the next tick re-fetches its usage. Returns `true` when a new pair persisted.
pub(crate) fn rotate_one(
    config: &crate::profile::ConfigHandle,
    name: &str,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
    sender: &OpResultSender,
    force: bool,
) -> bool {
    // Pre-stamp so the row shows a refresh spinner for the whole HTTP window;
    // rotate_one_inner clears the slot when it emits its OpResult.
    mark_activity(activity, name, ProfileActivity::Refreshing);
    let persisted = match rotate_one_inner(config, name, Some(activity), sender, force) {
        RotateOutcome::Persisted(true) => true,
        // Guard-fail never emits an OpResult; surface the failure + clear, exactly
        // as refresh_all's join loop does for a busy guard.
        RotateOutcome::GuardBusy => {
            let _ = sender.send(OpResult {
                name: name.to_string(),
                outcome: Err(anyhow::anyhow!("failed to acquire rotation lock")),
            });
            clear_activity(activity, name);
            false
        }
        // Persist/skip legs already emitted + cleared; clearing the pre-stamp again
        // is idempotent and covers the no-refresh-token early return.
        RotateOutcome::Persisted(false) => {
            clear_activity(activity, name);
            false
        }
    };
    if persisted && let Ok(mut q) = refetch.lock() {
        q.insert(name.to_string());
    }
    persisted
}

/// One-shot window prime for the CLI switch: if `name` is an opted-in OAuth
/// account, fire the kick (rotating once on a 401/429 via [`auto_start_kick`]).
/// No scheduler side channels and no cooldown — the CLI runs once and exits, so
/// there is no tick to debounce against. Returns whether the window opened.
///
/// The just-switched profile is active and freshly reconciled, so the diverged-
/// active guard the steady-state path needs doesn't apply here; opt-in + OAuth
/// is the whole gate.
pub(crate) fn prime_window(config: &crate::profile::ConfigHandle, name: &str) -> bool {
    let (access_token, refresh_token, expires_at) = {
        let cfg = config.lock().expect("config mutex poisoned");
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
            let refresh = profile.refresh_token().map(str::to_string);
            Ok(Some((token, refresh, profile.access_token_expires_at())))
        }) {
            Ok(Some(t)) => t,
            _ => return false,
        }
    };

    auto_start_kick(
        config,
        name,
        &access_token,
        refresh_token.as_deref(),
        expires_at,
        None,
    )
    .opened
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
