use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UsageWindow {
    pub(crate) utilization: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UsageInfo {
    pub(crate) five_hour: Option<UsageWindow>,
}

pub(crate) fn fetch(access_token: &str) -> Result<UsageInfo> {
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
