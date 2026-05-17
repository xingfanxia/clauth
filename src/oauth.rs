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
        .timeout_recv_response(Some(Duration::from_secs(8)))
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Refreshes OAuth tokens for every profile whose 5-hour usage window is
/// missing in the store — Claude Code's startup refresh is what starts that
/// timer, so a missing window means the timer hasn't begun yet.
///
/// Refresh tokens are single-use, so each successful call invalidates the
/// old pair; the rotated pair is persisted to credentials.json (active
/// profile's file is the symlink target Claude Code reads).
///
/// Returns the names of profiles whose tokens were successfully refreshed,
/// so the caller can re-fetch usage and confirm the timer is now visible.
/// Failures (network down, revoked refresh token) are swallowed: the
/// cached access token stays put.
pub(crate) fn refresh_missing_timers(config: &mut AppConfig, store: &UsageStore) -> Vec<String> {
    let snapshots: Vec<(String, String)> = {
        let usage = store.lock().ok();
        config
            .profiles
            .iter()
            .filter(|p| {
                usage
                    .as_ref()
                    .and_then(|s| s.get(&p.name))
                    .and_then(|u| u.five_hour.as_ref())
                    .is_none()
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
        .map(|(name, rt)| std::thread::spawn(move || (name, refresh(&rt))))
        .collect();

    let mut refreshed = Vec::new();
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
            refreshed.push(name);
        }
    }
    refreshed
}
