use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::Deserialize;

use crate::profile::{AppConfig, save_profile};
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

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
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
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{e}: {text}"))
}

/// Sends a 1-token Haiku message to start the 5-hour usage window. This is
/// what Claude Code effectively does on launch (just not visibly).
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
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// For every profile that opted in via `kick_timer = true` and currently has
/// no 5-hour usage window, refreshes its OAuth tokens (rotated pair saved to
/// disk) and fires a 1-token Haiku ping to start the window.
///
/// Failures (network down, revoked refresh token, ping rejected) are
/// swallowed: the cached state stays put.
///
/// Returns the names of profiles whose timer kick succeeded so the caller
/// can re-fetch usage and confirm the window now shows up.
pub(crate) fn kick_missing_timers(config: &mut AppConfig, store: &UsageStore) -> Vec<String> {
    let snapshots: Vec<(String, String)> = {
        let usage = store.lock().ok();
        config
            .profiles
            .iter()
            .filter(|p| p.kick_timer)
            .filter(|p| {
                // Skip profiles whose timer is already running.
                let info = usage.as_ref().and_then(|s| s.get(&p.name));
                let resets_at = info
                    .and_then(|u| u.five_hour.as_ref())
                    .and_then(|w| w.resets_at.as_ref());
                resets_at.is_none()
            })
            .filter_map(|p| {
                let token = p
                    .credentials
                    .as_ref()?
                    .claude_ai_oauth
                    .as_ref()?
                    .refresh_token
                    .as_ref()?
                    .clone();
                Some((p.name.clone(), token))
            })
            .collect()
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
        let Some(profile) = config.find_mut(&name) else {
            continue;
        };
        let Some(creds) = profile.credentials.as_mut() else {
            continue;
        };
        let Some(oauth) = creds.claude_ai_oauth.as_mut() else {
            continue;
        };
        oauth.access_token = tok.access_token;
        oauth.refresh_token = Some(tok.refresh_token);
        oauth.expires_at = Some(now_ms() + tok.expires_in * 1000);
        if let Some(scope) = tok.scope {
            oauth.scopes = Some(scope.split(' ').map(String::from).collect());
        }
        if save_profile(profile).is_ok() {
            kicked.push(name);
        }
    }
    kicked
}
