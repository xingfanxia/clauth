use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::lockorder::{RankedMutex, rank};

use super::scheduler::{ActivityStore, ProfileActivity, mark_activity};

const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/profile";

/// Re-fetch `/profile` (plan / rate-limit tier) at most once per hour per
/// profile. The tier rarely changes, so the steady usage poll reuses the cached
/// plan and only hits `/profile` on first load, after a 401 rotation, once the
/// hour lapses, or on a manual single-profile refresh (which expires this
/// clock). Halves the steady request volume against the rate-limited host.
const PROFILE_TTL_MS: u64 = 60 * 60 * 1000;

/// Per-profile epoch-ms of the last `/profile` fetch attempt — the TTL clock for
/// the policy above. Process-global and leaf-ranked: locked and released
/// entirely within the fetch decision, never nested under another tracked lock.
static PROFILE_FETCHED: LazyLock<RankedMutex<HashMap<String, u64>, rank::ProfileTtl>> =
    LazyLock::new(|| RankedMutex::new(HashMap::new()));

/// Minimum spacing between consecutive requests to the Anthropic OAuth endpoints
/// (`/usage` + `/profile`), enforced process-wide. Every profile authenticates
/// with the same OAuth client from one host, so the endpoint's rate limit is
/// effectively a shared bucket; serializing requests this far apart stops a
/// same-instant multi-profile burst (startup, refetch-queue drains) from tripping
/// a 429. Steady polling sits well below this rate, so it only bites on bursts.
const OAUTH_REQUEST_SPACING_MS: u64 = 5_000;

/// Earliest epoch-ms the next OAuth request may fire. Each caller reserves the
/// next free slot (advancing this by [`OAUTH_REQUEST_SPACING_MS`]) and sleeps
/// until it. Leaf-ranked and held only to reserve the slot — never across the
/// sleep or the HTTP round trip.
static NEXT_REQUEST_SLOT: LazyLock<RankedMutex<u64, rank::UsageThrottle>> =
    LazyLock::new(|| RankedMutex::new(0));

/// Pure slot reservation: from the current earliest-allowed slot and `now`,
/// return `(advanced_slot, wait_ms)` — the slot reserved for the next caller
/// (one [`OAUTH_REQUEST_SPACING_MS`] past this caller's fire time) and how long
/// this caller must wait for its own slot.
fn reserve_slot(current_slot: u64, now: u64) -> (u64, u64) {
    let fire_at = current_slot.max(now);
    (
        fire_at.saturating_add(OAUTH_REQUEST_SPACING_MS),
        fire_at.saturating_sub(now),
    )
}

/// Block until this caller's spacing slot, reserving the following slot for the
/// next caller. A poisoned lock skips throttling rather than stalling the fetch.
fn await_request_slot() {
    let now = now_ms();
    let wait_ms = {
        let Ok(mut slot) = NEXT_REQUEST_SLOT.lock() else {
            return;
        };
        let (next, wait) = reserve_slot(*slot, now);
        *slot = next;
        wait
    };
    if wait_ms > 0 {
        std::thread::sleep(Duration::from_millis(wait_ms));
    }
}

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

/// Canonical account tier, computed once at fetch time. The single source of
/// truth that `plan_label` / `endpoint_label` render from — collapses the old
/// four-field `PlanInfo` fan-out into one enum. `Serialize`/`Deserialize` keep it
/// in the `usage_cache.json` shape; a field rename simply misses → refetches.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub(crate) enum PlanTier {
    Max(#[serde(default)] Option<u16>),
    Pro,
    Team,
    Enterprise,
    Free,
    #[default]
    Unknown,
}

impl PlanTier {
    /// Reproduce the old `plan_label` classification exactly from the raw
    /// `/profile` fields. `rate_limit_tier` carries the Max multiplier when present.
    pub(crate) fn from_profile(
        org_type: Option<&str>,
        has_max: bool,
        has_pro: bool,
        rate_limit_tier: Option<&str>,
    ) -> Self {
        match org_type.unwrap_or("") {
            "claude_max" => PlanTier::Max(max_multiplier(rate_limit_tier)),
            "claude_pro" => PlanTier::Pro,
            "claude_team" | "claude_teams" => PlanTier::Team,
            "claude_enterprise" => PlanTier::Enterprise,
            "claude_free" | "free" => PlanTier::Free,
            "" => {
                if has_max {
                    PlanTier::Max(None)
                } else if has_pro {
                    PlanTier::Pro
                } else {
                    PlanTier::Unknown
                }
            }
            _ => PlanTier::Unknown,
        }
    }

    /// Map the OAuth token's `subscription_type` so a not-yet-fetched profile
    /// still shows a sane tier label. A missing value defaults to `Pro`,
    /// matching the old `endpoint_label` fallback (`unwrap_or("pro")`).
    pub(crate) fn from_subscription_type(s: Option<&str>) -> Self {
        match s.unwrap_or("pro") {
            "pro" => PlanTier::Pro,
            "max" => PlanTier::Max(None),
            "team" | "teams" => PlanTier::Team,
            "enterprise" => PlanTier::Enterprise,
            _ => PlanTier::Unknown,
        }
    }

    /// Same strings the old `plan_label` emitted, for every tier.
    pub(crate) fn display(&self) -> String {
        match self {
            PlanTier::Max(Some(n)) => format!("Claude Max {n}x"),
            PlanTier::Max(None) => "Claude Max".to_string(),
            PlanTier::Pro => "Claude Pro".to_string(),
            PlanTier::Team => "Claude Team".to_string(),
            PlanTier::Enterprise => "Claude Enterprise".to_string(),
            PlanTier::Free => "Claude Free".to_string(),
            PlanTier::Unknown => "Claude".to_string(),
        }
    }

    /// Compact tier label without the `Claude ` prefix, for contexts that
    /// already name the provider (e.g. the MCP inventory's `[anthropic, …]`).
    /// `None` for an unknown tier so callers can omit it entirely.
    pub(crate) fn short_label(&self) -> Option<String> {
        Some(match self {
            PlanTier::Max(Some(n)) => format!("Max {n}x"),
            PlanTier::Max(None) => "Max".to_string(),
            PlanTier::Pro => "Pro".to_string(),
            PlanTier::Team => "Team".to_string(),
            PlanTier::Enterprise => "Enterprise".to_string(),
            PlanTier::Free => "Free".to_string(),
            PlanTier::Unknown => return None,
        })
    }
}

/// Pull the trailing `Nx` multiplier out of a rate-limit tier like
/// `default_claude_max_5x` / `default_claude_max_20x`.
fn max_multiplier(tier: Option<&str>) -> Option<u16> {
    let tier = tier?;
    let last = tier.rsplit('_').next()?;
    last.strip_suffix('x').and_then(|m| {
        m.chars()
            .all(|c| c.is_ascii_digit())
            .then(|| m.parse().ok())?
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct PlanInfo {
    #[serde(default)]
    pub(crate) tier: PlanTier,
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
        // Provider window labels of the form `<n>h` / `<n>d` (e.g. z.ai's
        // `5h`/`7d`/`30d`) so any api-key account with a windowed limit gets the
        // same average pace + ideal-pace line as the OAuth windows.
        _ => parse_nh_nd_label(label),
    }
}

/// Parse a `"<n>h"` / `"<n>d"` window label into a duration in seconds. `None`
/// for any other shape.
fn parse_nh_nd_label(label: &str) -> Option<i64> {
    let (num, unit) = label.split_at(label.len().checked_sub(1)?);
    let n = num.parse::<i64>().ok().filter(|&n| n > 0)?;
    match unit {
        "h" => Some(n * 3600),
        "d" => Some(n * 86_400),
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

/// Average burn pace in %/day for `window`: utilization spread evenly over the
/// time elapsed since the window opened (`resets_at − duration`). Unlike the
/// recency-weighted recent-burn rate, this is anchored to the fixed window, so
/// it is unaffected by account rotation (which makes a per-profile history jump
/// to another account's utilization). `None` until `min_elapsed_secs` have
/// elapsed — a freshly opened window would otherwise divide by ~0 — or when the
/// window has no reset time or no fixed duration.
pub(crate) fn window_avg_pace_per_day(
    label: &str,
    window: &UsageWindow,
    now_secs: i64,
    min_elapsed_secs: i64,
) -> Option<f64> {
    let duration = window_duration_secs(label)?;
    let reset = iso_to_epoch_secs(window.resets_at.as_deref()?)?;
    let remaining = (reset - now_secs).clamp(0, duration);
    let elapsed = duration - remaining;
    if elapsed < min_elapsed_secs {
        return None;
    }
    Some(window.utilization / (elapsed as f64 / 86_400.0))
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

/// Parse a `retry-after` header value into a delay from now. Accepts the
/// delta-seconds form (`120`) and the IMF-fixdate HTTP-date form
/// (`Wed, 21 Oct 2015 07:28:00 GMT`); a past date yields `Duration::ZERO` and
/// anything else returns `None` — no usable hint.
pub(crate) fn parse_retry_after(value: &str) -> Option<Duration> {
    parse_retry_after_at(value, now_epoch_secs())
}

/// Pure core of [`parse_retry_after`] taking the reference instant, so the
/// HTTP-date branch is deterministic under test.
pub(crate) fn parse_retry_after_at(value: &str, now_secs: i64) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let target = httpdate_to_epoch_secs(value)?;
    Some(Duration::from_secs(
        target.saturating_sub(now_secs).max(0) as u64
    ))
}

/// Parse an HTTP-date in IMF-fixdate form (`Wed, 21 Oct 2015 07:28:00 GMT`) to
/// Unix epoch seconds. The obsolete RFC-850 / asctime forms and anything
/// malformed return `None`.
fn httpdate_to_epoch_secs(value: &str) -> Option<i64> {
    let mut parts = value.split_ascii_whitespace();
    parts.next()?; // day-of-week (e.g. "Wed,") — unused
    let day: i64 = parts.next()?.parse().ok()?;
    let month: i64 = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    if parts.next()? != "GMT" || parts.next().is_some() {
        return None;
    }
    let hour: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    let second: i64 = hms.next()?.parse().ok()?;
    if hms.next().is_some() || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
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

fn get_json(
    url: &str,
    access_token: &str,
    activity: Option<&ActivityStore>,
    name: &str,
) -> std::result::Result<String, FetchError> {
    await_request_slot();
    // The throttle wait is over and the request is about to leave the gate — flip
    // the spinner from `Queued` to `Fetching` so only the profile actually in
    // flight reads as fetching, not the whole batch waiting behind the spacing.
    if let Some(activity) = activity {
        mark_activity(activity, name, ProfileActivity::Fetching);
    }
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

/// Mark `name`'s plan stale so the next fetch re-pulls `/profile` — the manual
/// single-profile refresh (Usage `r` / action menu). A global "refresh all"
/// deliberately does not call this, so it keeps reusing the cached plan.
pub(crate) fn expire_profile_ttl(name: &str) {
    if let Ok(mut m) = PROFILE_FETCHED.lock() {
        m.remove(name);
    }
}

/// Decide whether to fetch `/profile` this round and stamp the attempt. Fetches
/// on a forced refresh (401 retry / manual single), on first load (no stamp yet,
/// incl. each process start), or once the hourly TTL lapses. Stamping on attempt
/// — success or failure alike — caps `/profile` at one hit per hour per profile,
/// so a persistently failing endpoint can't turn into a per-tick storm (the plan
/// is best-effort; a cold profile just shows no tier until the next hourly try).
fn take_profile_fetch(name: &str, force: bool, now: u64) -> bool {
    let fresh = PROFILE_FETCHED
        .lock()
        .ok()
        .and_then(|m| m.get(name).copied())
        .is_some_and(|t| now.saturating_sub(t) < PROFILE_TTL_MS);
    let want = force || !fresh;
    if want && let Ok(mut m) = PROFILE_FETCHED.lock() {
        m.insert(name.to_string(), now);
    }
    want
}

/// Fetch `/usage`; fetch `/profile` only when [`take_profile_fetch`] says so,
/// otherwise carry `prev_plan` forward. `force_profile` bypasses the TTL (used
/// for the post-401-rotation retry). A `/profile` failure never drops usage —
/// it falls back to `prev_plan`.
pub(super) fn fetch_raw(
    name: &str,
    access_token: &str,
    prev_plan: Option<PlanInfo>,
    force_profile: bool,
    activity: Option<&ActivityStore>,
) -> std::result::Result<UsageInfo, FetchError> {
    let usage_text = get_json(USAGE_ENDPOINT, access_token, activity, name)?;
    let raw: RawUsage = serde_json::from_str(&usage_text).map_err(|_| FetchError::Parse)?;

    let plan = if take_profile_fetch(name, force_profile, now_ms()) {
        get_json(PROFILE_ENDPOINT, access_token, activity, name)
            .ok()
            .and_then(|text| serde_json::from_str::<RawProfile>(&text).ok())
            .map(|p| {
                let org = p.organization.as_ref();
                PlanInfo {
                    tier: PlanTier::from_profile(
                        org.and_then(|o| o.organization_type.as_deref()),
                        p.account.as_ref().is_some_and(|a| a.has_claude_max),
                        p.account.as_ref().is_some_and(|a| a.has_claude_pro),
                        org.and_then(|o| o.rate_limit_tier.as_deref()),
                    ),
                }
            })
            // Profile leg failed (transient / 401 on a stale token) — keep the
            // prior plan rather than dropping it from the snapshot.
            .or(prev_plan)
    } else {
        prev_plan
    };

    Ok(UsageInfo {
        plan,
        five_hour: raw.five_hour,
        seven_day: raw.seven_day,
        seven_day_opus: raw.seven_day_opus,
        seven_day_sonnet: raw.seven_day_sonnet,
        extra_usage: raw.extra_usage,
    })
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

    let days = days_from_civil(year, month, day);
    Some(days * 86400 + hour * 3600 + minute * 60 + second - tz_offset_secs)
}

/// Howard Hinnant's days-from-civil: days since 1970-01-01 for a proleptic
/// Gregorian `(year, month, day)`. Shared by the ISO-8601 and HTTP-date parsers.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
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
