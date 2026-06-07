//! Third-party API provider integration.
//!
//! Recognises providers by base URL and fetches provider-specific statistics
//! for display on the Usage and Setup tabs. Each provider lives in its own
//! submodule; this module owns the shared model, HTTP helper, and disk cache.
//!
//! Adding a provider:
//! 1. Create `src/providers/<name>.rs` with `DISPLAY_NAME`, `matches_base_url`,
//!    and `fetch` (mirror [`deepseek`]).
//! 2. Add a variant to [`Provider`] and wire it into `from_base_url`,
//!    `display_name`, and [`fetch_third_party_usage`]'s match arms.
//!
//! No render-layer changes needed — [`ThirdPartyStats`] carries generic
//! [`StatRow`]s that [`crate::tui::render::usage`] renders uniformly.

mod deepseek;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── Provider ────────────────────────────────────────────────────────────────────

/// Recognised third-party API provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Provider {
    DeepSeek,
}

impl Provider {
    /// Match a base URL to a known provider. `None` when unrecognised.
    pub(crate) fn from_base_url(url: &str) -> Option<Self> {
        if deepseek::matches_base_url(url) {
            Some(Self::DeepSeek)
        } else {
            None
        }
    }

    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::DeepSeek => deepseek::DISPLAY_NAME,
        }
    }
}

/// `true` when `url` is exactly `base` or `base` followed by a real URL
/// delimiter — path `/`, port `:`, query `?`, or fragment `#`
/// (`https://api.deepseek.com`, `.../v1`, `...:443`), never a host extension
/// (`https://api.deepseek.com.evil.tld`) — a bare `starts_with` would claim
/// those and send the profile's API key to the real provider endpoint.
///
/// The scheme + host are compared case-insensitively (hosts are
/// case-insensitive per RFC 3986). `url` is lowercased; `base` is lowercased
/// defensively so a future caller passing mixed-case still matches.
fn url_matches_host(url: &str, base: &str) -> bool {
    let url = url.to_ascii_lowercase();
    let base = base.to_ascii_lowercase();
    match url.strip_prefix(&base) {
        Some("") => true,
        Some(rest) => rest.starts_with(['/', ':', '?', '#']),
        None => false,
    }
}

/// Fetch usage for a recognised provider. Dispatches to the right endpoint.
pub(crate) fn fetch_third_party_usage(
    provider: Provider,
    api_key: &str,
) -> Result<ThirdPartyStats, ThirdPartyError> {
    match provider {
        Provider::DeepSeek => deepseek::fetch(api_key),
    }
}

// ── Stats model ─────────────────────────────────────────────────────────────────

/// Provider-agnostic statistics for the Usage tab.
///
/// Each provider's fetch function builds one of these from its API response.
/// The render layer iterates [`rows`](Self::rows) — no per-provider branching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ThirdPartyStats {
    /// `false` means the account can't make API calls (e.g. balance exhausted).
    pub(crate) is_available: bool,
    /// Display rows in source order.
    pub(crate) rows: Vec<StatRow>,
}

impl ThirdPartyStats {
    fn from_rows(rows: Vec<StatRow>) -> Self {
        Self {
            is_available: true,
            rows,
        }
    }

    fn unavailable(reason: &str) -> Self {
        Self {
            is_available: false,
            rows: vec![StatRow {
                label: String::new(),
                value: reason.to_string(),
                kind: StatRowKind::Danger,
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StatRow {
    /// Left-hand label. Empty for single-line messages (e.g. "unavailable").
    pub(crate) label: String,
    /// Right-hand value.
    pub(crate) value: String,
    pub(crate) kind: StatRowKind,
}

/// Visual weight of a row in the Usage tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StatRowKind {
    /// Section header (bold, TEXT_DIM + bold).
    Heading,
    /// Normal key:value.
    Body,
    /// Danger-coloured (e.g. "balance unavailable").
    Danger,
    /// Dim / faint text.
    Faint,
}

// ── Error ───────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) enum ThirdPartyError {
    /// Provider returned a non-429 >=400 status (e.g. 401 bad key). The caller
    /// doesn't branch on the code — third-party profiles have no chain to
    /// rotate — so it collapses to a cache-fallback like `Network`/`Parse`.
    Status,
    /// HTTP 429. `retry_after` is the server's `retry-after` header in
    /// delta-seconds form (the HTTP-date form is treated as absent), used to
    /// defer this profile's next slot — mirrors the OAuth fetch path.
    RateLimited {
        retry_after: Option<std::time::Duration>,
    },
    Network,
    Parse,
}

// ── HTTP ────────────────────────────────────────────────────────────────────────

fn get_json(url: &str, api_key: &str) -> Result<String, ThirdPartyError> {
    let mut response = crate::usage::http_agent()
        .get(url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .call()
        .map_err(|_| ThirdPartyError::Network)?;
    let status = response.status().as_u16();
    if status == 429 {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(crate::usage::parse_retry_after);
        return Err(ThirdPartyError::RateLimited { retry_after });
    }
    if status >= 400 {
        return Err(ThirdPartyError::Status);
    }
    response
        .body_mut()
        .read_to_string()
        .map_err(|_| ThirdPartyError::Network)
}

// ── Disk cache ──────────────────────────────────────────────────────────────────

fn cache_path(profile_name: &str) -> Option<PathBuf> {
    crate::profile::profile_dir(profile_name)
        .ok()
        .map(|p| p.join("third_party_cache.json"))
}

pub(crate) fn load_third_party_disk_cache(name: &str) -> Option<ThirdPartyStats> {
    cache_path(name).and_then(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        serde_json::from_str::<ThirdPartyStats>(&text).ok()
    })
}

pub(crate) fn write_third_party_disk_cache(name: &str, stats: &ThirdPartyStats) {
    let Some(path) = cache_path(name) else {
        return;
    };
    let Ok(json) = serde_json::to_string(stats) else {
        return;
    };
    // Atomic tmp + rename — a torn plain write would parse-fail and read as no cache.
    let _ = crate::profile::atomic_write(&path, json);
}

#[cfg(test)]
#[path = "../../tests/inline/providers.rs"]
mod tests;
