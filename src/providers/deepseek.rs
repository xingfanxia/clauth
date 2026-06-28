//! DeepSeek provider — balance stats from `GET /user/balance`.
//!
//! Wire shape per <https://api-docs.deepseek.com/api/get-user-balance>.

use serde::Deserialize;

use super::{StatRow, StatRowKind, ThirdPartyError, ThirdPartyStats, url_matches_host};

pub(super) const DISPLAY_NAME: &str = "DeepSeek";

pub(super) const ORIGIN: &str = "https://api.deepseek.com";
const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

pub(super) fn matches_base_url(url: &str) -> bool {
    url_matches_host(url, ORIGIN)
}

pub(super) fn fetch(api_key: &str) -> Result<ThirdPartyStats, ThirdPartyError> {
    let text = super::get_json(BALANCE_URL, api_key)?;
    let raw: DeepSeekResponse = serde_json::from_str(&text).map_err(|_| ThirdPartyError::Parse)?;
    Ok(stats(&raw))
}

/// Pure response → display-rows mapping, separated from HTTP for testability.
fn stats(raw: &DeepSeekResponse) -> ThirdPartyStats {
    if !raw.is_available {
        return ThirdPartyStats::unavailable("balance unavailable");
    }

    let mut rows: Vec<StatRow> = Vec::new();
    for info in &raw.balance_infos {
        rows.push(StatRow {
            label: format!("{} balance", info.currency),
            value: String::new(),
            kind: StatRowKind::Heading,
        });
        rows.push(StatRow {
            label: "total".to_string(),
            value: format!("{} {}", info.total_balance, info.currency),
            kind: StatRowKind::Body,
        });
        rows.push(StatRow {
            label: "granted".to_string(),
            value: format!("{} {}", info.granted_balance, info.currency),
            kind: StatRowKind::Body,
        });
        rows.push(StatRow {
            label: "topped up".to_string(),
            value: format!("{} {}", info.topped_up_balance, info.currency),
            kind: StatRowKind::Body,
        });
    }
    ThirdPartyStats::from_rows(rows)
}

// ── Wire types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct DeepSeekResponse {
    is_available: bool,
    #[serde(default)]
    balance_infos: Vec<DeepSeekBalance>,
}

#[derive(Debug, Clone, Deserialize)]
struct DeepSeekBalance {
    currency: String,
    total_balance: String,
    granted_balance: String,
    topped_up_balance: String,
}

#[cfg(test)]
#[path = "../../tests/inline/providers_deepseek.rs"]
mod tests;
