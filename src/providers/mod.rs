//! Third-party API provider integration.
//!
//! Recognises providers by base URL and fetches provider-specific statistics
//! for display on the Usage and Setup tabs. Each provider lives in its own
//! submodule; this module owns the shared model, HTTP helper, and disk cache.
//!
//! Adding a provider:
//! 1. Create `src/providers/<name>.rs` with `DISPLAY_NAME`, `matches_base_url`,
//!    and `fetch` (mirror [`deepseek`] for balances, [`zai`] for limit bars +
//!    per-model token rows).
//! 2. Add a variant to [`Provider`] and wire it into `from_base_url`,
//!    `display_name`, and [`fetch_third_party_usage`]'s match arms.
//!
//! No render-layer changes needed — [`ThirdPartyStats`] carries provider-agnostic
//! [`UsageBar`]s (percentage windows) and [`StatRow`]s (text), which
//! [`crate::tui::render::usage`] renders uniformly. Unknown api-key providers go
//! through [`generic`]'s best-effort scanner, which sets `best_effort` so the UI
//! invites a bug report.

mod deepseek;
mod generic;
mod zai;

use serde::{Deserialize, Serialize};

// ── Provider ────────────────────────────────────────────────────────────────────

/// Recognised third-party API provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Provider {
    DeepSeek,
    Zai,
}

impl Provider {
    /// Match a base URL to a known provider. `None` when unrecognised.
    pub(crate) fn from_base_url(url: &str) -> Option<Self> {
        if deepseek::matches_base_url(url) {
            Some(Self::DeepSeek)
        } else if zai::matches_base_url(url) {
            Some(Self::Zai)
        } else {
            None
        }
    }

    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::DeepSeek => deepseek::DISPLAY_NAME,
            Self::Zai => zai::DISPLAY_NAME,
        }
    }

    /// Canonical `scheme://host` this provider's requests target — the per-host
    /// request-pacing key (see [`ThirdPartyTarget::throttle_key`]).
    fn origin(self) -> &'static str {
        match self {
            Self::DeepSeek => deepseek::ORIGIN,
            Self::Zai => zai::ORIGIN,
        }
    }
}

/// What a third-party scheduler entry fetches against: a recognised provider
/// (typed fetch) or an unrecognised api-key endpoint (generic discovery + scan).
#[derive(Debug, Clone)]
pub(crate) enum ThirdPartyTarget {
    Known(Provider),
    /// Generic api-key endpoint: usage is discovered + scanned at this base_url's
    /// API origin (same host the key already authorises for completions).
    Generic {
        base_url: String,
    },
}

impl ThirdPartyTarget {
    /// Origin (`scheme://host`) used as the per-host request-pacing key, so accounts
    /// on the same endpoint serialize while distinct hosts run in parallel. A generic
    /// base URL with no parseable scheme falls back to the raw string — still a
    /// stable per-account key.
    pub(crate) fn throttle_key(&self) -> String {
        match self {
            Self::Known(provider) => provider.origin().to_string(),
            Self::Generic { base_url } => api_origin(base_url).unwrap_or_else(|| base_url.clone()),
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

/// Derive the API origin (`scheme://host[:port]`) from a base URL, dropping any
/// path/query/fragment. The generic usage engine probes candidate endpoints
/// against this origin only — the api_key never travels to a different host than
/// the one it already authorises. `None` when the `://` scheme delimiter is absent.
pub(crate) fn api_origin(base_url: &str) -> Option<String> {
    let scheme_end = base_url.find("://")?;
    let after = &base_url[scheme_end + 3..];
    let auth_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    Some(format!(
        "{}://{}",
        &base_url[..scheme_end],
        &after[..auth_end]
    ))
}

/// Fetch usage for a third-party target. `hint` is the endpoint path that last
/// yielded data (read from the in-memory store by the caller); only the generic
/// arm uses it, to keep steady state at one request.
pub(crate) fn fetch_third_party_usage(
    target: &ThirdPartyTarget,
    api_key: &str,
    hint: Option<&str>,
) -> Result<ThirdPartyStats, ThirdPartyError> {
    match target {
        ThirdPartyTarget::Known(provider) => match provider {
            Provider::DeepSeek => deepseek::fetch(api_key),
            Provider::Zai => zai::fetch(api_key),
        },
        ThirdPartyTarget::Generic { base_url } => generic::fetch(base_url, api_key, hint),
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
    /// Text display rows in source order.
    pub(crate) rows: Vec<StatRow>,
    /// Percentage-based usage windows rendered as bars (e.g. z.ai limits).
    /// Empty for scalar/balance providers that use `rows` instead.
    #[serde(default)]
    pub(crate) bars: Vec<UsageBar>,
    /// Plan/tier label for the header (e.g. "pro"), when the response carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) plan: Option<String>,
    /// Endpoint path that last yielded this data — the generic fetcher reuses it
    /// next tick to skip re-probing. Recognised providers leave this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) endpoint: Option<String>,
    /// `true` when this came from the best-effort generic scanner (unknown
    /// provider) rather than a typed integration — the render layer shows a
    /// "looks wrong? open an issue" hint. Typed providers leave it `false`.
    #[serde(default)]
    pub(crate) best_effort: bool,
}

/// One percentage-based usage window for bar rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UsageBar {
    pub(crate) label: String,
    /// 0..=100.
    pub(crate) pct: f64,
    /// ISO-8601 reset timestamp when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resets_at: Option<String>,
    /// Absolute amount consumed in the window, when the response carries one
    /// (z.ai `currentValue`). Rendered as the `x` of the bar's trailing `x / y`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) used: Option<f64>,
    /// Absolute window ceiling, when the response carries one — an explicit
    /// total/limit field, or `used + remaining` as a robust fallback (z.ai has
    /// no total field but carries `currentValue` + `remaining`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) total: Option<f64>,
}

impl ThirdPartyStats {
    fn from_rows(rows: Vec<StatRow>) -> Self {
        Self {
            is_available: true,
            rows,
            bars: Vec::new(),
            plan: None,
            endpoint: None,
            best_effort: false,
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
            bars: Vec::new(),
            plan: None,
            endpoint: None,
            best_effort: false,
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
//
// Per-profile JSON cache lives in `crate::profile_cache` (shared with the OAuth
// usage layer); this layer only contributes its filename + concrete type.

#[cfg(test)]
#[path = "../../tests/inline/providers.rs"]
mod tests;
