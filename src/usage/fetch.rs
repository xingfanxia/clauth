use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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

/// Display labels for each usage window — the single source of truth.
pub(crate) const LABEL_5H: &str = "5h";
pub(crate) const LABEL_7D: &str = "7d";
pub(crate) const LABEL_7D_SONNET: &str = "7d sonnet";
pub(crate) const LABEL_7D_OPUS: &str = "7d opus";

impl UsageInfo {
    /// All available windows as `(label, &UsageWindow)` pairs.
    pub(crate) fn windows(&self) -> Vec<(&'static str, &UsageWindow)> {
        let mut out = Vec::new();
        if let Some(w) = &self.five_hour {
            out.push((LABEL_5H, w));
        }
        if let Some(w) = &self.seven_day {
            out.push((LABEL_7D, w));
        }
        if let Some(w) = &self.seven_day_sonnet {
            out.push((LABEL_7D_SONNET, w));
        }
        if let Some(w) = &self.seven_day_opus {
            out.push((LABEL_7D_OPUS, w));
        }
        out
    }

    /// Most representative weekly window: Max returns per-model windows, Pro returns `seven_day`.
    pub(crate) fn weekly_window(&self) -> Option<&UsageWindow> {
        self.seven_day
            .as_ref()
            .or(self.seven_day_sonnet.as_ref())
            .or(self.seven_day_opus.as_ref())
    }
}

/// Nominal length of the rolling window named by `label`, in seconds. `None`
/// for labels with no fixed window (e.g. the monthly extra-credits bar).
pub(crate) fn window_duration_secs(label: &str) -> Option<i64> {
    match label {
        LABEL_5H => Some(5 * 3600),
        LABEL_7D | LABEL_7D_SONNET | LABEL_7D_OPUS => Some(7 * 86_400),
        _ => None,
    }
}

/// Ideal-pace percentage (0..=100) for a usage window at `now_secs`: the share
/// of the window already elapsed. Usage spread evenly across the window tracks
/// this line, so a fill past it is ahead of pace and a fill behind it is under
/// pace. `None` when the window has no reset time or no fixed duration.
pub(crate) fn ideal_pace_pct(label: &str, window: &UsageWindow, now_secs: i64) -> Option<f64> {
    let duration = window_duration_secs(label)?;
    let reset = iso_to_epoch_secs(window.resets_at.as_deref()?)?;
    let remaining = (reset - now_secs).clamp(0, duration);
    let elapsed = duration - remaining;
    Some(elapsed as f64 / duration as f64 * 100.0)
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

/// HTTP layer error. `Status` carries an HTTP code so the fetch path can
/// distinguish a 401 (refresh + retry) from a connection blip (cache); a 429
/// gets its own variant carrying the server's `retry-after` hint (rate-limited,
/// cache — never rotate, defer the next attempt).
pub(super) enum FetchError {
    Status(u16),
    /// HTTP 429. `retry_after` is the server's `retry-after` header when
    /// present in delta-seconds form (the HTTP-date form is treated as absent).
    RateLimited {
        retry_after: Option<Duration>,
    },
    Network,
    Parse,
}

/// Parse a `retry-after` header value in delta-seconds form. The HTTP-date
/// form (and anything else non-numeric) returns `None` — no hint.
pub(crate) fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        .timeout_recv_response(Some(Duration::from_secs(8)))
        // ureq 3 defaults non-2xx to `Err(Error::StatusCode)`; our callers read
        // the status off the `Ok` response (401 → rotate, 429 → retry-after).
        // Without this flag those branches are unreachable and every HTTP error
        // collapses into `Network`.
        .http_status_as_error(false)
        .build()
        .into()
});

/// Shared HTTP agent for usage-style GETs (also used by `crate::providers`).
/// Status codes arrive on the `Ok` response — see the builder comment.
pub(crate) fn http_agent() -> &'static ureq::Agent {
    &AGENT
}

fn get_json(url: &str, access_token: &str) -> std::result::Result<String, FetchError> {
    let mut response = AGENT
        .get(url)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
        .map_err(|_| FetchError::Network)?;
    let status = response.status().as_u16();
    if status == 429 {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        return Err(FetchError::RateLimited { retry_after });
    }
    if status >= 400 {
        return Err(FetchError::Status(status));
    }
    response
        .body_mut()
        .read_to_string()
        .map_err(|_| FetchError::Network)
}

pub(super) fn fetch_raw(access_token: &str) -> std::result::Result<UsageInfo, FetchError> {
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

/// Read the on-disk usage cache for `name`. Returns `None` when no cache exists.
pub(crate) fn load_disk_cache(name: &str) -> Option<UsageInfo> {
    cache_path(name).and_then(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        serde_json::from_str::<UsageInfo>(&text).ok()
    })
}

/// Write the live response to disk for use on future restart or API failure.
pub(crate) fn write_disk_cache(name: &str, info: &UsageInfo) {
    let Some(path) = cache_path(name) else {
        return;
    };
    let Ok(json) = serde_json::to_string(info) else {
        return;
    };
    // `atomic_write_600` creates any missing parent dir at 0o700 itself; a plain
    // `create_dir_all` here would win the race and leave it world-readable.
    let _ = crate::profile::atomic_write_600(&path, json.as_bytes());
}

fn cache_path(profile_name: &str) -> Option<PathBuf> {
    // Use `profile_dir` (override-aware) rather than raw `dirs::home_dir` so
    // tests never touch the real `~/.clauth`.
    crate::profile::profile_dir(profile_name)
        .ok()
        .map(|p| p.join("usage_cache.json"))
}

/// Epoch-ms of the usage cache's last write (≈ the last successful live fetch),
/// or `None` when no cache exists. Lets startup skip a boot-time fetch when the
/// on-disk numbers are still within one refresh interval.
pub(crate) fn cache_mtime_ms(name: &str) -> Option<u64> {
    let modified = std::fs::metadata(cache_path(name)?).ok()?.modified().ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse ISO-8601 timestamp (e.g. `2026-05-17T14:20:00.121699+00:00`) into Unix epoch seconds.
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
        // Accept `±HH`, `±HHMM`, `±HH:MM`.
        let digits: String = after_frac[1..].chars().filter(|&c| c != ':').collect();
        if after_frac[1..]
            .chars()
            .any(|c| c != ':' && !c.is_ascii_digit())
        {
            return None;
        }
        let (tz_h, tz_m): (i64, i64) = match digits.len() {
            2 => (digits.parse().ok()?, 0),
            4 => (digits[0..2].parse().ok()?, digits[2..4].parse().ok()?),
            _ => return None,
        };
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

/// Format Unix epoch seconds as ISO-8601 UTC (`YYYY-MM-DDTHH:MM:SS+00:00`) —
/// the shape [`iso_to_epoch_secs`] parses. Negative inputs clamp to epoch 0.
pub(crate) fn epoch_secs_to_iso(secs: i64) -> String {
    let secs = secs.max(0);
    let s = secs % 60;
    let mi = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Civil-from-days — the inverse of days-from-civil in `iso_to_epoch_secs`.
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}+00:00")
}

pub(crate) fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Format seconds as `Nd Nh`, `Nh Nm`, or `Nm`; returns `"now"` for ≤0.
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

#[cfg(test)]
#[path = "../../tests/inline/fetch.rs"]
mod tests;
