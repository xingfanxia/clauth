use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UsageWindow {
    pub(crate) utilization: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UsageInfo {
    pub(crate) five_hour: Option<UsageWindow>,
}

pub(crate) fn fetch_cached(profile_name: &str, access_token: &str) -> Option<UsageInfo> {
    let cache = cache_path(profile_name);
    match fetch(access_token) {
        Ok(info) => {
            if let Some(ref path) = cache
                && let Ok(json) = serde_json::to_string(&info)
            {
                let _ = std::fs::write(path, json);
            }
            Some(info)
        }
        Err(_) => cache.and_then(|p| {
            let text = std::fs::read_to_string(p).ok()?;
            serde_json::from_str(&text).ok()
        }),
    }
}

fn fetch(access_token: &str) -> Result<UsageInfo> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        .timeout_recv_response(Some(Duration::from_secs(8)))
        .build()
        .into();

    let text = agent
        .get(ENDPOINT)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{e}"))
}

fn cache_path(profile_name: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".clauth")
            .join("profiles")
            .join(profile_name)
            .join("usage_cache.json")
    })
}
