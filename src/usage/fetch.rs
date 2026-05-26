use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::lock::with_state_lock;
use crate::profile::{ClaudeCredentials, OAuthToken, atomic_write, clauth_dir, profile_dir};

const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/profile";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UsageWindow {
    pub(crate) utilization: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resets_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ExtraUsage {
    #[serde(default)]
    pub(crate) is_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) monthly_limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) used_credits: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) utilization: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct PlanInfo {
    /// e.g. "claude_max", "claude_pro", "claude_team", "claude_enterprise"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) organization_type: Option<String>,
    /// e.g. "default_claude_max_5x", "default_claude_max_20x"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rate_limit_tier: Option<String>,
    #[serde(default)]
    pub(crate) has_max: bool,
    #[serde(default)]
    pub(crate) has_pro: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct UsageInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) plan: Option<PlanInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) five_hour: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) seven_day: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) seven_day_opus: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) seven_day_sonnet: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) extra_usage: Option<ExtraUsage>,
}

impl UsageInfo {
    /// Picks the most representative weekly window. Max accounts return
    /// per-model windows (`seven_day_sonnet`/`seven_day_opus`); Pro returns
    /// the model-agnostic `seven_day`.
    pub(crate) fn weekly_window(&self) -> Option<&UsageWindow> {
        self.seven_day
            .as_ref()
            .or(self.seven_day_sonnet.as_ref())
            .or(self.seven_day_opus.as_ref())
    }
}

#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    five_hour: Option<UsageWindow>,
    #[serde(default)]
    seven_day: Option<UsageWindow>,
    #[serde(default)]
    seven_day_opus: Option<UsageWindow>,
    #[serde(default)]
    seven_day_sonnet: Option<UsageWindow>,
    #[serde(default)]
    extra_usage: Option<ExtraUsage>,
}

#[derive(Deserialize)]
struct RawProfile {
    #[serde(default)]
    account: Option<RawProfileAccount>,
    #[serde(default)]
    organization: Option<RawProfileOrg>,
}

#[derive(Deserialize)]
struct RawProfileAccount {
    #[serde(default)]
    has_claude_max: bool,
    #[serde(default)]
    has_claude_pro: bool,
}

#[derive(Deserialize)]
struct RawProfileOrg {
    #[serde(default)]
    organization_type: Option<String>,
    #[serde(default)]
    rate_limit_tier: Option<String>,
}

/// HTTP layer error. `Status` carries an HTTP code so the rotation path can
/// distinguish 401/429 (refresh + retry) from a connection blip (cache).
pub(crate) enum FetchError {
    Status(u16),
    Network,
    Parse,
}

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        .timeout_recv_response(Some(Duration::from_secs(8)))
        .build()
        .into()
});

fn get_json(url: &str, access_token: &str) -> std::result::Result<String, FetchError> {
    let mut response = AGENT
        .get(url)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
        .map_err(|_| FetchError::Network)?;
    let status = response.status().as_u16();
    if status >= 400 {
        return Err(FetchError::Status(status));
    }
    response
        .body_mut()
        .read_to_string()
        .map_err(|_| FetchError::Network)
}

pub(crate) fn fetch_raw(access_token: &str) -> std::result::Result<UsageInfo, FetchError> {
    let usage_text = get_json(USAGE_ENDPOINT, access_token)?;
    let raw: RawUsage = serde_json::from_str(&usage_text).map_err(|_| FetchError::Parse)?;

    // Profile is best-effort: a stale token may 401 on /profile while /usage
    // still serves cached numbers. A profile failure shouldn't drop usage.
    let plan = get_json(PROFILE_ENDPOINT, access_token)
        .ok()
        .and_then(|text| serde_json::from_str::<RawProfile>(&text).ok())
        .map(|p| PlanInfo {
            organization_type: p
                .organization
                .as_ref()
                .and_then(|o| o.organization_type.clone()),
            rate_limit_tier: p
                .organization
                .as_ref()
                .and_then(|o| o.rate_limit_tier.clone()),
            has_max: p.account.as_ref().is_some_and(|a| a.has_claude_max),
            has_pro: p.account.as_ref().is_some_and(|a| a.has_claude_pro),
        });

    Ok(UsageInfo {
        plan,
        five_hour: raw.five_hour,
        seven_day: raw.seven_day,
        seven_day_opus: raw.seven_day_opus,
        seven_day_sonnet: raw.seven_day_sonnet,
        extra_usage: raw.extra_usage,
    })
}

/// Read the on-disk usage cache for `name`. Returns `Some(info)` when a
/// snapshot is available, `None` when no cache exists. The caller maps this
/// to a `FetchStatus` (`Cached` when the bytes load, `Failed` when they
/// don't) so this module stays free of scheduler types.
pub(crate) fn load_disk_cache(name: &str) -> Option<UsageInfo> {
    cache_path(name).and_then(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        serde_json::from_str::<UsageInfo>(&text).ok()
    })
}

/// Write the live response to disk so a future restart (or a tick where the
/// API is unreachable) can still surface numbers.
pub(crate) fn write_disk_cache(name: &str, info: &UsageInfo) {
    let Some(path) = cache_path(name) else {
        return;
    };
    let Ok(json) = serde_json::to_string(info) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

/// Persist a rotated OAuth pair into `~/.clauth/profiles/<name>/credentials.json`
/// and bump `profiles.toml`'s mtime so any process polling that file picks up
/// the new tokens. `subscription_type` is preserved from the prior file when
/// present — the rotation response never includes it.
pub(crate) fn persist_oauth_token(name: &str, oauth: &OAuthToken) -> Result<()> {
    with_state_lock(|| {
        let path = profile_dir(name)?.join("credentials.json");
        let mut creds: ClaudeCredentials = if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            serde_json::from_str(&content)?
        } else {
            ClaudeCredentials {
                claude_ai_oauth: None,
            }
        };
        let prior_sub = creds
            .claude_ai_oauth
            .as_ref()
            .and_then(|o| o.subscription_type.clone());
        let merged = OAuthToken {
            subscription_type: prior_sub,
            ..oauth.clone()
        };
        creds.claude_ai_oauth = Some(merged);
        atomic_write(&path, serde_json::to_string_pretty(&creds)?)?;

        // Touching profiles.toml advances its mtime, which the main thread's
        // `reload_if_state_changed` watches. Without this, an in-session
        // rotation wouldn't propagate into AppConfig until the next external
        // edit, leaving subsequent fetches reusing the old access token.
        let state_path = clauth_dir()?.join("profiles.toml");
        if let Ok(content) = std::fs::read_to_string(&state_path) {
            let _ = atomic_write(&state_path, content);
        }
        Ok(())
    })
}

fn cache_path(profile_name: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".clauth")
            .join("profiles")
            .join(profile_name)
            .join("usage_cache.json")
    })
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Time helpers ──────────────────────────────────────────────────────────────

/// Parses an ISO-8601 timestamp like `2026-05-17T14:20:00.121699+00:00` into
/// seconds since the Unix epoch. Returns None on any format deviation.
pub(crate) fn iso_to_epoch_secs(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year: i64 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: i64 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: i64 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    let hour: i64 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let minute: i64 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let second: i64 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;

    let tail = &s[19..];
    let after_frac = if let Some(rest) = tail.strip_prefix('.') {
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        &rest[end..]
    } else {
        tail
    };
    let tz_offset_secs: i64 = if after_frac.is_empty() || after_frac.starts_with('Z') {
        0
    } else {
        let sign = match after_frac.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        if after_frac.len() < 6 {
            return None;
        }
        let tz_h: i64 = after_frac[1..3].parse().ok()?;
        let tz_m: i64 = after_frac[4..6].parse().ok()?;
        sign * (tz_h * 3600 + tz_m * 60)
    };

    // Howard Hinnant's days-from-civil — yields days since 1970-01-01.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;

    Some(days * 86400 + hour * 3600 + minute * 60 + second - tz_offset_secs)
}

pub(crate) fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Formats a duration in seconds as a tight `Nd Nh`, `Nh Nm`, or `Nm` string.
/// Returns `now` for zero or negative.
pub(crate) fn humanize_duration(secs: i64) -> String {
    if secs <= 0 {
        return "now".to_string();
    }
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if days > 0 {
        format!("{}d {}h", days, hours % 24)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins % 60)
    } else {
        format!("{}m", mins.max(1))
    }
}
