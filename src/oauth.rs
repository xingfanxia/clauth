use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::Deserialize;

use crate::lock::with_state_lock;
use crate::profile::{AppConfig, save_app_state, save_profile};
use crate::usage::UsageStore;

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

/// Refuse to rekick a profile until this long after its last kick. Sized just
/// under the 5-hour window so a kick whose follow-up usage fetch failed
/// (network blip, stale endpoint) doesn't cause us to re-fire on every
/// launch, while still allowing a fresh kick once the window has elapsed.
const KICK_COOLDOWN_MS: u64 = 4 * 3600 * 1000 + 30 * 60 * 1000;

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    #[serde(default)]
    scope: Option<String>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        .timeout_recv_response(Some(Duration::from_secs(15)))
        .build()
        .into()
}

fn refresh(refresh_token: &str) -> Result<TokenResponse> {
    let body = serde_json::to_string(&serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    }))?;

    let text = agent()
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

    agent()
        .post(MESSAGES_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .send(&body)
        .map_err(crate::ureq_error::into_anyhow)?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Refreshes every profile's OAuth token pair (rotated pair saved to disk).
/// Mirrors what Claude Code does silently on launch — minus the kick.
///
/// Profiles without a stored refresh token are skipped. Network or revocation
/// failures are swallowed per-profile; cached state stays put for those.
///
/// Returns the names of profiles whose token rotation succeeded so the caller
/// can target follow-up work (usage re-fetch, kick) at the same set.
pub(crate) fn refresh_all(config: &mut AppConfig) -> Vec<String> {
    let snapshots: Vec<(String, String)> = config
        .profiles
        .iter()
        .filter_map(|p| {
            let rt = p
                .credentials
                .as_ref()?
                .claude_ai_oauth
                .as_ref()?
                .refresh_token
                .clone()?;
            Some((p.name.clone(), rt))
        })
        .collect();

    if snapshots.is_empty() {
        return Vec::new();
    }

    let handles: Vec<_> = snapshots
        .into_iter()
        .map(|(name, rt)| std::thread::spawn(move || (name, refresh(&rt))))
        .collect();

    let mut refreshed = Vec::new();
    for h in handles {
        let Ok((name, Ok(tok))) = h.join() else {
            continue;
        };
        let saved = with_state_lock(|| {
            let Some(profile) = config.find_mut(&name) else {
                return Ok::<_, anyhow::Error>(false);
            };
            let Some(creds) = profile.credentials.as_mut() else {
                return Ok(false);
            };
            let Some(oauth) = creds.claude_ai_oauth.as_mut() else {
                return Ok(false);
            };
            oauth.access_token = tok.access_token;
            oauth.refresh_token = Some(tok.refresh_token);
            oauth.expires_at = Some((now_ms() + tok.expires_in * 1000) as i64);
            if let Some(scope) = tok.scope {
                oauth.scopes = Some(scope.split_whitespace().map(String::from).collect());
            }
            Ok(save_profile(profile).is_ok())
        })
        .unwrap_or(false);
        if saved {
            refreshed.push(name);
        }
    }
    refreshed
}

/// For every profile that opted in via `kick_timer = true` and currently has
/// no 5-hour usage window, refreshes its OAuth tokens (rotated pair saved to
/// disk) and fires a 1-token Haiku ping to start the window.
///
/// Returns the names of profiles whose timer kick succeeded so the caller
/// can re-fetch usage and confirm the window now shows up.
pub(crate) fn kick_missing_timers(config: &mut AppConfig, store: &UsageStore) -> Vec<String> {
    // Claim cooldown slots under the lock BEFORE any network work. A competing
    // clauth process that starts up a moment later will observe our recorded
    // `last_kick_at` and skip the same profile, so the refresh token rotates
    // exactly once even when two instances race startup.
    //
    // Holding the lock during the OAuth/messages HTTP round trips would stall
    // every other instance for seconds, so we release between claim and work.
    let snapshots: Vec<(String, String)> = match with_state_lock(|| {
        let now = now_ms();
        let mut claimed = Vec::new();
        for profile in &config.profiles {
            if !profile.kick_timer {
                continue;
            }
            let resets_at = {
                let usage = store.lock().ok();
                usage
                    .as_ref()
                    .and_then(|s| s.get(&profile.name))
                    .and_then(|u| u.five_hour.as_ref())
                    .and_then(|w| w.resets_at.clone())
            };
            if resets_at.is_some() {
                continue;
            }
            let last = config
                .state
                .last_kick_at
                .get(&profile.name)
                .copied()
                .unwrap_or(0);
            if now.saturating_sub(last) < KICK_COOLDOWN_MS {
                continue;
            }
            let Some(token) = profile
                .credentials
                .as_ref()
                .and_then(|c| c.claude_ai_oauth.as_ref())
                .and_then(|o| o.refresh_token.as_ref())
                .cloned()
            else {
                continue;
            };
            claimed.push((profile.name.clone(), token));
        }

        for (name, _) in &claimed {
            config.state.last_kick_at.insert(name.clone(), now);
        }
        if !claimed.is_empty() {
            let _ = save_app_state(&config.state);
        }
        Ok::<_, anyhow::Error>(claimed)
    }) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let handles: Vec<_> = snapshots
        .into_iter()
        .map(|(name, rt)| {
            std::thread::spawn(move || {
                let tok = match refresh(&rt) {
                    Ok(t) => t,
                    Err(e) => return (name, Err(e)),
                };
                if let Err(e) = kick(&tok.access_token) {
                    return (name, Err(e));
                }
                (name, Ok(tok))
            })
        })
        .collect();

    let mut kicked = Vec::new();
    for h in handles {
        let Ok((name, Ok(tok))) = h.join() else {
            continue;
        };
        let succeeded = with_state_lock(|| {
            let Some(profile) = config.find_mut(&name) else {
                return Ok::<_, anyhow::Error>(false);
            };
            let Some(creds) = profile.credentials.as_mut() else {
                return Ok(false);
            };
            let Some(oauth) = creds.claude_ai_oauth.as_mut() else {
                return Ok(false);
            };
            oauth.access_token = tok.access_token;
            oauth.refresh_token = Some(tok.refresh_token);
            oauth.expires_at = Some((now_ms() + tok.expires_in * 1000) as i64);
            if let Some(scope) = tok.scope {
                oauth.scopes = Some(scope.split_whitespace().map(String::from).collect());
            }
            Ok(save_profile(profile).is_ok())
        })
        .unwrap_or(false);
        if succeeded {
            kicked.push(name);
        }
    }
    kicked
}
