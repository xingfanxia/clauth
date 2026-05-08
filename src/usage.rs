use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) type UsageStore = Arc<Mutex<HashMap<String, UsageInfo>>>;
pub(crate) type TokenList = Arc<Mutex<Vec<(String, String)>>>;

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
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
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

/// Fetches usage for every (name, token) pair in parallel and writes results
/// into the shared store. Blocks until all fetches complete.
pub(crate) fn fetch_all_into(tokens: &[(String, String)], store: &UsageStore) {
    let handles: Vec<_> = tokens
        .iter()
        .map(|(name, token)| {
            let n = name.clone();
            let t = token.clone();
            std::thread::spawn(move || (n.clone(), fetch_cached(&n, &t)))
        })
        .collect();

    let Ok(mut s) = store.lock() else {
        return;
    };
    for h in handles {
        if let Ok((name, Some(info))) = h.join() {
            s.insert(name, info);
        }
    }
}

/// Spawns a background thread that re-fetches usage for the current token
/// list every 30s and writes results into the shared store. The token list
/// is read fresh each tick so renames and new profiles are picked up.
pub(crate) fn spawn_refresher(tokens: TokenList, store: UsageStore) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(REFRESH_INTERVAL);
            let snapshot = match tokens.lock() {
                Ok(t) => t.clone(),
                Err(_) => continue,
            };
            for (name, token) in &snapshot {
                if let Some(info) = fetch_cached(name, token)
                    && let Ok(mut s) = store.lock()
                {
                    s.insert(name.clone(), info);
                }
            }
        }
    });
}
