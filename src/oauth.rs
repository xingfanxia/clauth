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
    ANTHROPIC_ORIGIN, ActivityStore, OpResult, OpResultSender, ProfileActivity, RefetchQueue,
    await_request_slot, clear_activity, mark_activity, now_ms,
};

/// Anthropic's OAuth token endpoint. Same one Claude Code uses on startup to
/// mint an access token from the stored refresh token.
const TOKEN_ENDPOINT: &str = "https://api.anthropic.com/v1/oauth/token";

/// Token endpoint for the interactive authorization-code exchange. Paired with
/// the `platform.claude.com` authorize host the current Claude Code binary uses
/// (see `oauth_login`). Refresh stays on [`TOKEN_ENDPOINT`] (proven working).
const LOGIN_TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";

/// UUID of the "Claude Code" OAuth application; required for refresh and the
/// interactive login (`oauth_login` builds the authorize URL with it).
pub(crate) const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

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

/// Build a safe error for a **2xx** token-endpoint body that failed to parse
/// into [`TokenResponse`]. The body still holds the live access+refresh tokens,
/// so neither it nor serde's `Display` (which echoes the offending scalar — a
/// possible token substring) may reach an error surfaced on `clauth login` or a
/// TUI toast; a leaked token is account takeover. The value-free channel
/// (category + position + status + length) is pinned by
/// `token_parse_error_redacts_the_2xx_body`.
fn token_parse_error(e: serde_json::Error, status: u16, body_len: usize) -> anyhow::Error {
    let kind = match e.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "malformed json",
        serde_json::error::Category::Data => "unexpected shape",
        serde_json::error::Category::Eof => "truncated",
    };
    anyhow::anyhow!(
        "token endpoint returned HTTP {status} but its body did not parse as a token \
         response ({kind} at line {}, column {}); {body_len} bytes withheld \
         (contains live credentials)",
        e.line(),
        e.column(),
    )
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

/// Cap a raw HTTP error body to its first line, max 200 chars, before it
/// reaches a user-facing toast — an upstream error page must not flood a
/// one-line surface.
fn http_error(status: u16, body: &str) -> anyhow::Error {
    let detail: String = body
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    if detail.is_empty() {
        anyhow::anyhow!("HTTP {status}")
    } else {
        anyhow::anyhow!("HTTP {status}: {detail}")
    }
}

/// A token-refresh failure, split so the AUTH-1 gate can tell a *permanently*
/// revoked/invalid refresh token (quarantine the account — `clauth login` is the
/// only fix) from a *transient* network/429/5xx blip (refuse this one switch,
/// retry next tick — never quarantine a healthy account on a hiccup).
pub(crate) enum RefreshError {
    /// The token endpoint rejected the refresh token — it is revoked or
    /// invalid. HTTP 400/401, or a 403 whose body confirms `invalid_grant`.
    Invalid(String),
    /// Transport failure, 429, 5xx, or an unconfirmed 403 (WAF/geo/challenge
    /// blocks answer 403 too) — the refresh token may still be good.
    Transient(anyhow::Error),
}

impl From<RefreshError> for anyhow::Error {
    fn from(e: RefreshError) -> Self {
        match e {
            RefreshError::Invalid(msg) => anyhow::anyhow!(msg),
            RefreshError::Transient(e) => e,
        }
    }
}

/// [`refresh`] preserving the permanent-vs-transient distinction the AUTH-1 gate
/// needs. A 400/401 from the token endpoint means the refresh token is
/// revoked/invalid (quarantine — OAuth2 reports a dead refresh token as a 400
/// `invalid_grant`; some proxies answer 401). A 403 is terminal only when the
/// body actually confirms `invalid_grant`: WAF/geo/challenge blocks answer 403
/// too, and quarantining on one would take a healthy account out of rotation.
/// A transport error, 429, or 5xx is transient (retry, never quarantine).
pub(crate) fn refresh_result(
    refresh_token: &str,
) -> std::result::Result<TokenResponse, RefreshError> {
    let body = serde_json::to_string(&serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    }))
    .map_err(|e| RefreshError::Transient(e.into()))?;

    let mut response = AGENT
        .post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .send(&body)
        .map_err(|e| RefreshError::Transient(anyhow::Error::from(e)))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| RefreshError::Transient(anyhow::Error::from(e)))?;
    if refresh_rejection_is_terminal(status, &text) {
        return Err(RefreshError::Invalid(http_error(status, &text).to_string()));
    }
    if status >= 400 {
        return Err(RefreshError::Transient(http_error(status, &text)));
    }

    serde_json::from_str(&text)
        .map_err(|e| RefreshError::Transient(token_parse_error(e, status, text.len())))
}

/// Whether a token-endpoint rejection means the refresh token itself is dead
/// (quarantine) rather than the request being blocked (retry). Extracted pure
/// so the truth table is pinned offline
/// (`refresh_rejection_terminal_truth_table`).
fn refresh_rejection_is_terminal(status: u16, body: &str) -> bool {
    matches!(status, 400 | 401) || (status == 403 && body.contains("invalid_grant"))
}

pub(crate) fn refresh(refresh_token: &str) -> Result<TokenResponse> {
    refresh_result(refresh_token).map_err(Into::into)
}

/// Exchange an authorization code (from the interactive loopback login in
/// `oauth_login`) for an OAuth token pair. Uses the same client + HTTP agent as
/// [`refresh`], against [`LOGIN_TOKEN_ENDPOINT`] (paired with the authorize host
/// the current Claude Code binary uses). `redirect_uri` MUST byte-match the one
/// sent to the authorize endpoint, and `state` echoes the value round-tripped
/// through the browser.
pub(crate) fn exchange_code(
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    state: &str,
) -> Result<TokenResponse> {
    let body = serde_json::to_string(&serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect_uri,
        "code_verifier": code_verifier,
        "client_id": CLIENT_ID,
        "state": state,
    }))?;

    let mut response = AGENT
        .post(LOGIN_TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .send(&body)
        .map_err(anyhow::Error::from)?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(anyhow::Error::from)?;
    if status >= 400 {
        return Err(http_error(status, &text));
    }

    serde_json::from_str(&text).map_err(|e| token_parse_error(e, status, text.len()))
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
/// Claude Code does silently on launch. Shares the `api.anthropic.com` per-host
/// request-spacing slot so a same-instant multi-profile window-reset doesn't burst
/// `/v1/messages`.
fn kick(access_token: &str) -> std::result::Result<(), KickError> {
    await_request_slot(ANTHROPIC_ORIGIN);
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
        .map_err(|e| KickError::Other(anyhow::Error::from(e)))?
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
/// rotation happened; the pair is live even when `opened` is false, because the
/// previous single-use refresh token is already spent and dropping it would
/// strand the profile.
#[must_use]
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
    // The refresh already spent the old single-use token, so this pair is now the
    // only usable one — carry it back even when the persist below fails, or the
    // caller's live snapshot keeps the dead token and 400s every tick until a
    // restart adopts the staged sidecar. The retry kick may still fail (`opened`
    // false), but a minted pair must always propagate (see `KickResult`).
    let rotated = Some((access.clone(), Some(new_refresh)));
    if apply_rotated_tokens_locked(config, name, tok).is_err() {
        return KickResult {
            opened: false,
            rotated,
        };
    }
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
/// A live session is ALWAYS skipped — never rotated, not even on a user-forced
/// rotate. It owns the single-use refresh chain and advances it itself, so our
/// stored token is stale; refreshing it would 400 ("refresh token not found or
/// invalid"). `force` (a rotate-all concern, see `rotation_candidates`) governs
/// only the diverged-active profile, never this safety skip.
///
/// HTTP/persist leg emits one `OpResult { kind: Refreshing }` and clears the
/// activity slot. Returns [`RotateOutcome::GuardBusy`] without emitting an
/// `OpResult` when the lock can't be acquired (slot never pre-stamped here;
/// `refresh_all` pre-stamps and clears it). No-refresh-token / skipped-live-
/// session legs return [`RotateOutcome::Persisted(false)`] silently (the live-
/// session case is messaged up front by the single-rotate caller).
fn rotate_one_inner(
    config: &crate::profile::ConfigHandle,
    name: &str,
    activity: Option<&ActivityStore>,
    sender: &OpResultSender,
) -> RotateOutcome {
    let Ok(_rotation_guard) = RotationGuard::acquire(name) else {
        return RotateOutcome::GuardBusy;
    };
    let token = {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let cfg = config.lock().expect("config mutex poisoned");
        with_state_lock(|| {
            // A live `clauth start` session owns this profile's single-use OAuth
            // chain and refreshes it itself, so our stored refresh token is
            // already spent — rotating it 400s ("refresh token not found or
            // invalid"). Never rotate a live session; this in-guard check is
            // authoritative (a session that won the race stamped its PID before
            // releasing the RotationGuard). Skipping returns Persisted(false).
            if has_live_session(name) {
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
/// Diverged-active profiles are included only when `force`; live-session
/// profiles are ALWAYS excluded (a running session owns the single-use chain,
/// so our stored token is stale — rotating it 400s, `force` or not).
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
            // Never rotate a profile with a live `clauth start` session — its
            // chain is owned and advanced by that session; force does not apply.
            if has_live_session(&p.name) {
                return None;
            }
            Some((p.name.to_string(), p.refresh_token()?.to_string()))
        })
        .collect()
}

/// Refreshes every profile's OAuth token pair (rotated pair saved to disk).
/// Mirrors what Claude Code does silently on launch — minus the kick.
///
/// Profiles without a stored refresh token are skipped, as are profiles with a
/// live `clauth start` session (always — they own their own chain). Network/
/// revocation failures are swallowed per-profile; cached state stays put.
/// `force` bypasses only the diverged-active guard.
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
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
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
                // single-use token mid-rotation. A session that started after
                // `rotation_candidates` snapshotted is caught by the in-guard
                // `has_live_session` skip inside (returns Persisted(false)).
                let outcome = rotate_one_inner(&config, &name, Some(&activity), &sender);
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

/// Rotate a single profile's OAuth token pair — one [`refresh_all`] worker leg,
/// scoped to `name` (the action-menu "rotate tokens" on the focused account).
/// Same discipline: `rotate_one_inner` holds the per-profile RotationGuard across
/// the HTTP window with a `has_live_session` skip, so a profile with a live
/// `clauth start` session is never rotated (its stored token is stale — the
/// caller refuses up front; this is the backstop). On success the profile is
/// pushed onto `refetch` so the next tick re-fetches its usage. Returns `true`
/// when a new pair persisted.
pub(crate) fn rotate_one(
    config: &crate::profile::ConfigHandle,
    name: &str,
    refetch: &RefetchQueue,
    activity: &ActivityStore,
    sender: &OpResultSender,
) -> bool {
    // Pre-stamp so the row shows a refresh spinner for the whole HTTP window;
    // rotate_one_inner clears the slot when it emits its OpResult.
    mark_activity(activity, name, ProfileActivity::Refreshing);
    let persisted = match rotate_one_inner(config, name, Some(activity), sender) {
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
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
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
    #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
    let mut cfg = config.lock().expect("config mutex poisoned");
    // Rotation coherence (#1): a rotation of the ACTIVE profile revokes the
    // single-use refresh token the macOS Keychain copy carries — the running
    // `claude` (which re-reads the Keychain per request) would sign out at
    // that stale token's expiry while every clauth copy stays green (observed
    // on-device 2026-07-07). The mirror DECISION and the creds snapshot are
    // made under the locked section below, so the written pair is exactly the
    // persisted one; the `/usr/bin/security` shell-out itself runs after the
    // flock is released (it can hang up to its 20 s kill deadline, and the
    // global state flock must never be held across a subprocess — before this
    // function the locked section contained only fast disk writes). In-process
    // switches stay excluded for the whole window by the config mutex held
    // across this function.
    #[cfg(target_os = "macos")]
    let mut mirror: Option<crate::profile::ClaudeCredentials> = None;
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
        // Pre-rotation access token, kept for the Keychain-mirror gate below:
        // it tells "the live file is a stale mirror of OUR OWN chain" apart
        // from a genuinely foreign CC re-login.
        #[cfg(target_os = "macos")]
        let old_access = oauth.access_token.clone();
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
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled() && cfg.is_active(name) {
            if live_login_is_foreign(name, &old_access) {
                eprintln!(
                    "clauth: rotated '{name}' but the live login diverged (a re-login clauth \
                     doesn't own) — Keychain left untouched; resolve the divergence in the TUI"
                );
            } else {
                mirror = cfg.find(name).and_then(|p| p.credentials.as_ref()).cloned();
            }
        }
        Ok(())
    })?;
    // A failed state flock surfaces as the `Err` from `with_state_lock` above,
    // so a poisoned/unavailable lock never looks like a successful rotation.
    // A mirror failure is loud but non-fatal: the rotation itself is durable,
    // and the next rotation or switch retries the write.
    #[cfg(target_os = "macos")]
    if let Some(creds) = mirror
        && let Err(e) = crate::keychain::keychain_write(&creds)
    {
        eprintln!(
            "clauth: rotated '{name}' but the Keychain mirror failed: {e:#} — a \
             running claude signs out when its old token expires; run `clauth {name}` \
             to reinstall"
        );
    }
    Ok(())
}

/// Adopt the live session's OWN token rotation instead of fighting it
/// (rotation coherence — the future-proof half). The running `claude` and
/// clauth hold ONE single-use refresh family; whoever refreshes first revokes
/// the other. Rather than racing, concede: CC maintains
/// `~/.claude/.credentials.json` as a regular-file mirror of its Keychain
/// login (rewritten at least on every CC launch), a prompt-free read path to
/// CC's current pair. When that mirror holds a FRESHER pair for the SAME
/// account, adopt it into the profile store — no refresh spent — so clauth
/// stays correct whatever refresh schedule a future Claude Code ships.
///
/// Gates, in order — every one must pass:
///   * `name` is the ACTIVE profile (only its chain is shared with a live CC);
///   * the live path classifies [`crate::claude::LinkState::Diverged`]
///     (`LinkedTo` = mirror equals the store, nothing to adopt);
///   * the mirror pair carries a refresh token and a STRICTLY LATER expiry
///     than the store (never adopt sideways or backwards);
///   * identity: the mirror token's account uuid (via `identity`, injected so
///     the gate is testable offline; prod passes `usage::fetch_account_uuid`)
///     matches the profile's cached uuid — or, when no uuid is cached yet, the
///     STORED token's own uuid fetched now (only possible while it still
///     works). Unprovable identity refuses the adopt: a live login belonging
///     to a different account (a manual CC `/login`) must never be captured
///     into this profile unattended — that stays the TUI divergence flow's
///     job.
///
/// On success the mirror uuid is cached (`ACCOUNT_ID_CACHE_FILE`), so later
/// adopts can verify identity even when the stored token is already dead.
/// The Keychain is NOT written here — in this state CC minted the pair, so
/// the Keychain and mirror are already the fresh truth; only our store lags.
///
/// Returns the adopted `(access, refresh)` pair so the caller can sync its
/// in-memory `TokenList` exactly like every other rotation site — without it,
/// the next poll would run on the superseded entry, spend the revoked refresh
/// token, and fail on the very account the adopt just saved.
///
/// `_rotation_guard` is proof the caller holds this profile's per-profile
/// rotation lock: the adopt mutates the same stored credential fields as a
/// refresh persist (`rotate_one_inner`), so both writers must serialize on
/// the same [`crate::runtime::RotationGuard`], not just the state flock.
/// Taken by reference because the flock is not reentrant — the refresh-failure
/// call site already holds the guard when it retries the adopt.
pub(crate) fn try_adopt_live_rotation(
    config: &crate::profile::ConfigHandle,
    name: &str,
    _rotation_guard: &crate::runtime::RotationGuard,
    identity: &dyn Fn(&str) -> Option<String>,
) -> Option<(String, Option<String>)> {
    use crate::profile_cache::{ACCOUNT_ID_CACHE_FILE, load_profile_cache, write_profile_cache};

    // Snapshot the store side under the config lock, then drop it — the
    // identity fetches below are HTTP and must never hold the mutex.
    let (stored_access, stored_expires) = {
        let Ok(cfg) = config.lock() else { return None };
        if !cfg.is_active(name) {
            return None;
        }
        let p = cfg.find(name)?;
        (
            p.access_token().map(str::to_string),
            p.access_token_expires_at(),
        )
    };

    if !matches!(
        crate::claude::classify_credentials_link(name),
        Ok(crate::claude::LinkState::Diverged)
    ) {
        return None;
    }
    let Ok(Some(live)) = crate::claude::read_claude_credentials() else {
        return None;
    };
    let live_oauth = live.claude_ai_oauth.as_ref()?;
    live_oauth.refresh_token.as_ref()?;
    let (Some(live_expires), Some(stored_expires)) = (live_oauth.expires_at, stored_expires) else {
        return None;
    };
    if live_expires <= stored_expires {
        return None;
    }

    // Identity anchor: cached uuid, else the stored token's own uuid while it
    // still authenticates. No anchor → refuse (identity unprovable).
    let expected: Option<String> = load_profile_cache::<String>(name, ACCOUNT_ID_CACHE_FILE)
        .or_else(|| {
            let alive = (now_ms() as i64) < stored_expires;
            match (&stored_access, alive) {
                (Some(tok), true) => identity(tok),
                _ => None,
            }
        });
    let Some(expected) = expected else {
        eprintln!(
            "clauth: live login for '{name}' is newer but its identity can't be proven \
             (no cached account id and the stored token is dead) — not adopting; \
             resolve in the TUI or re-run clauth login {name}"
        );
        return None;
    };
    let live_id = identity(&live_oauth.access_token)?;
    // A blank uuid is shape drift, not an identity — two blanks matching each
    // other must never prove two tokens are the same account.
    if live_id.trim().is_empty() || expected.trim().is_empty() {
        return None;
    }
    if live_id != expected {
        eprintln!(
            "clauth: live login for '{name}' belongs to a DIFFERENT account — not adopting; \
             capture it via the TUI divergence flow if that was intentional"
        );
        return None;
    }

    // Persist under config mutex + state flock, re-checking the gates that
    // could have moved during the HTTP window (an interleaved switch or a
    // rotation that already advanced the store past the mirror).
    #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
    let mut cfg = config.lock().expect("config mutex poisoned");
    let adopted = with_state_lock(|| {
        if !cfg.is_active(name) {
            return Ok(false);
        }
        let Some(profile) = cfg.find_mut(name) else {
            return Ok(false);
        };
        if profile
            .access_token_expires_at()
            .is_none_or(|cur| live_expires <= cur)
        {
            return Ok(false);
        }
        profile.credentials = Some(live.clone());
        save_profile(profile)?;
        Ok::<bool, anyhow::Error>(true)
    })
    .unwrap_or(false);
    if !adopted {
        return None;
    }
    // The adopted pair proves the chain is alive, so a standing `auth_broken`
    // is stale — the flag was set while CC held the fresher pair. Same lift as
    // the scheduler's `carry_external_rotation` (inlined here because the
    // config guard is already held); without it, an active recovered by a
    // CC-side re-login stays excluded from the fallback walk and refused as a
    // switch target until a manual `clauth login`.
    if cfg.set_auth_broken(name, false) {
        eprintln!("clauth: '{name}' re-authenticated — auth_broken cleared");
        let _ = crate::profile::save_app_state(&cfg.state);
    }
    write_profile_cache(name, ACCOUNT_ID_CACHE_FILE, &live_id);
    eprintln!(
        "clauth: adopted the live session's rotated login for '{name}' \
         (the running claude refreshed first — no token spent)"
    );
    Some((
        live_oauth.access_token.clone(),
        live_oauth.refresh_token.clone(),
    ))
}

/// Whether the live `.credentials.json` holds a login clauth does NOT own —
/// i.e. genuinely [`crate::claude::LinkState::Diverged`] and not merely a
/// stale regular-file mirror of this profile's own pre-rotation pair. On
/// macOS Claude Code rewrites the live file as a regular-file copy of the
/// Keychain, so the moment a rotation lands, `classify_credentials_link`
/// reports Diverged against the NEW stored token even though the live login
/// is still our own chain one step behind — that stale-mirror case must still
/// be mirrored, or the coherence write would skip exactly when it matters.
/// Only a live token matching NEITHER the new nor the pre-rotation pair is
/// foreign (a real CC re-login); an unreadable/unclassifiable state is
/// treated as foreign so a state we cannot understand is never overwritten.
#[cfg(target_os = "macos")]
fn live_login_is_foreign(name: &str, old_access: &str) -> bool {
    match crate::claude::classify_credentials_link(name) {
        Ok(crate::claude::LinkState::LinkedTo) | Ok(crate::claude::LinkState::Missing) => false,
        Ok(crate::claude::LinkState::Diverged) => {
            let live = crate::claude::read_claude_credentials().ok().flatten();
            let live_token = live.as_ref().and_then(|c| c.access_token());
            !live_token.is_some_and(|t| !t.is_empty() && t == old_access)
        }
        Err(_) => true,
    }
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

/// Grace window (ms): a token with less than this much life left is treated as
/// expiring, so the AUTH-1 gate refreshes it *before* install rather than
/// letting the freshly-switched session hit a 401.
const AUTH_GATE_GRACE_MS: i64 = 60_000;

/// Outcome of the pre-install auth gate ([`ensure_installable`]).
pub(crate) enum AuthGate {
    /// Safe to install the target's stored credentials as-is: a third-party
    /// (api-key) profile, an OAuth token with real life left, or a profile whose
    /// live `clauth start` session keeps its own chain fresh.
    Ready,
    /// The target's expiring OAuth token was refreshed and the rotated pair
    /// persisted; install the refreshed credentials.
    Refreshed,
    /// The target's refresh token is revoked/invalid — the profile is marked
    /// `auth_broken` (persisted). The caller MUST NOT install: a dead token in
    /// the Keychain logs out every running `claude` (Incident C).
    Broken,
    /// A transient failure (network/429/5xx, a busy rotation lock, or a poisoned
    /// mutex) blocked a needed refresh. Do not install now; retry on a later
    /// tick. The account is NOT quarantined.
    Transient(anyhow::Error),
}

/// Pre-install auth gate (AUTH-1 / Incident C). Installing `name`'s stored
/// credentials into the macOS Keychain instantly re-authenticates every running
/// `claude` on this machine, so a dead token must never be installed: this
/// refreshes an expiring OAuth token before install, quarantines a revoked one
/// ([`AuthGate::Broken`]), and passes healthy or third-party targets through.
/// Every branch is pinned by the `gate_*` tests in this module's test file.
///
/// `refresher` is injected so the gate is testable offline (real callers pass
/// [`refresh_result`]). The config mutex is never held across the HTTP refresh,
/// and the per-profile `RotationGuard` wraps the refresh so a live session or
/// sibling worker cannot double-spend the single-use token.
pub(crate) fn ensure_installable(
    config: &crate::profile::ConfigHandle,
    name: &str,
    refresher: impl Fn(&str) -> std::result::Result<TokenResponse, RefreshError>,
) -> AuthGate {
    // Cheap pre-check WITHOUT the rotation guard: non-OAuth and
    // comfortably-live tokens install as-is. Token data read here is
    // discarded — only the post-guard re-read may feed the refresher (a
    // pre-guard snapshot can go stale the moment a sibling rotation runs).
    match oauth_shape(config, name) {
        Err(gate) => return gate,
        Ok((expires_at, _, flagged)) if !expiring(expires_at, flagged) => {
            return AuthGate::Ready;
        }
        Ok(_) => {}
    }

    // RotationGuard across the HTTP window (single-use double-spend guard),
    // acquired with no config lock held. A busy guard means a live session or
    // sibling worker is already on this chain — refuse this switch and retry.
    let Ok(_guard) = RotationGuard::acquire(name) else {
        return AuthGate::Transient(anyhow::anyhow!(
            "'{name}' rotation lock busy; retry after the in-flight refresh"
        ));
    };
    // Authoritative under the guard: a live `clauth start` session owns and
    // advances this profile's single-use chain and keeps the Keychain fresh, so
    // refreshing here would 400 the session — install as-is.
    if has_live_session(name) {
        return AuthGate::Ready;
    }
    gate_under_guard(config, name, refresher)
}

/// The target's auth shape — `(access-token expiry, refresh token, standing
/// auth_broken flag)` — read under the config lock and released before
/// returning, so no caller ever holds the mutex across an HTTP refresh. `Err`
/// carries the gate verdict for the non-OAuth / unknown-profile / poisoned
/// cases.
#[allow(
    clippy::type_complexity,
    reason = "one-shot triple, named at both call sites"
)]
fn oauth_shape(
    config: &crate::profile::ConfigHandle,
    name: &str,
) -> std::result::Result<(Option<i64>, Option<String>, bool), AuthGate> {
    let Ok(cfg) = config.lock() else {
        return Err(AuthGate::Transient(anyhow::anyhow!(
            "config mutex poisoned"
        )));
    };
    let Some(profile) = cfg.find(name) else {
        // Unknown profile: nothing to gate — the switch itself surfaces
        // "Profile not found".
        return Err(AuthGate::Ready);
    };
    if !profile.is_oauth() {
        // Third-party (api-key) profiles carry no OAuth token to expire.
        return Err(AuthGate::Ready);
    }
    Ok((
        profile.access_token_expires_at(),
        profile.refresh_token().map(str::to_string),
        cfg.is_auth_broken(name),
    ))
}

/// Unknown expiry → treated as not-expiring (mirrors `auto_start_kick`):
/// install as-is and let the lazy 401→rotate path handle a surprise expiry.
/// A standing `auth_broken` flag overrides the clock: the chain's last refresh
/// terminally failed, so a still-future `expires_at` proves nothing
/// (server-side revocation outlives the stored clock). Route it through the
/// refresher — a recovered chain comes back `Refreshed` and lifts the flag, a
/// dead one confirms `Broken`.
fn expiring(expires_at: Option<i64>, flagged: bool) -> bool {
    flagged || expires_at.is_some_and(|exp| (now_ms() as i64) + AUTH_GATE_GRACE_MS >= exp)
}

/// The refresh leg, entered only with the [`RotationGuard`] held. Re-reads the
/// auth shape UNDER the guard — between the pre-check and guard acquisition a
/// sibling rotation may have spent the single-use refresh token and persisted
/// a new pair, and refreshing from that stale snapshot would 400 and wrongly
/// quarantine a healthy login. This function takes no token arguments, so
/// post-guard decisions structurally cannot reuse pre-guard data.
fn gate_under_guard(
    config: &crate::profile::ConfigHandle,
    name: &str,
    refresher: impl Fn(&str) -> std::result::Result<TokenResponse, RefreshError>,
) -> AuthGate {
    let (expires_at, refresh_token, flagged) = match oauth_shape(config, name) {
        Err(gate) => return gate,
        Ok(shape) => shape,
    };
    if !expiring(expires_at, flagged) {
        // A sibling refreshed while we acquired the guard — the stored pair is
        // fresh; install it as-is instead of double-spending the old chain.
        return AuthGate::Ready;
    }
    let Some(rt) = refresh_token else {
        // Expiring OAuth token with no refresh token — unrecoverable without a
        // re-login.
        mark_auth_broken(config, name, true);
        return AuthGate::Broken;
    };

    match refresher(&rt) {
        Ok(tok) => {
            if apply_rotated_tokens_locked(config, name, tok).is_err() {
                return AuthGate::Transient(anyhow::anyhow!(
                    "refreshed '{name}' but failed to persist the rotated tokens"
                ));
            }
            // A successful refresh clears any prior quarantine.
            mark_auth_broken(config, name, false);
            AuthGate::Refreshed
        }
        Err(RefreshError::Invalid(_)) => {
            mark_auth_broken(config, name, true);
            AuthGate::Broken
        }
        Err(RefreshError::Transient(e)) => AuthGate::Transient(e),
    }
}

/// Set or clear a profile's persisted `auth_broken` flag and save. Best-effort:
/// a failed save leaves the in-memory flag as set for this run (re-applied on the
/// next attempt). Locks `config` (outer) then `save_app_state` takes the state
/// flock (inner) — the established save order.
pub(crate) fn mark_auth_broken(config: &crate::profile::ConfigHandle, name: &str, broken: bool) {
    if let Ok(mut cfg) = config.lock()
        && cfg.set_auth_broken(name, broken)
    {
        // Log the transition only — guarded by `set_auth_broken`'s changed-return
        // (pinned by `set_auth_broken_reports_transitions_and_is_idempotent`) so a
        // dropped login leaves one stderr line, never a per-tick repeat.
        if broken {
            eprintln!(
                "clauth: login for '{name}' expired — refresh token dead; \
                 flagged auth_broken. Recover with: clauth login {name}"
            );
        } else {
            eprintln!("clauth: '{name}' re-authenticated — auth_broken cleared");
        }
        let _ = crate::profile::save_app_state(&cfg.state);
    }
}

#[cfg(test)]
#[path = "../tests/inline/oauth.rs"]
mod tests;
