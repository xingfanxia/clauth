use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
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
enum FetchError {
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

fn fetch_raw(access_token: &str) -> std::result::Result<UsageInfo, FetchError> {
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

/// Read the on-disk usage cache for `name`. Returns `(Some, status)` when a
/// snapshot is available, `(None, Failed)` when no cache exists.
fn load_disk_cache(name: &str, status: FetchStatus) -> (Option<UsageInfo>, FetchStatus) {
    let loaded = cache_path(name).and_then(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        serde_json::from_str::<UsageInfo>(&text).ok()
    });
    match loaded {
        Some(info) => (Some(info), status),
        None => (None, FetchStatus::Failed),
    }
}

/// Write the live response to disk so a future restart (or a tick where the
/// API is unreachable) can still surface numbers.
fn write_disk_cache(name: &str, info: &UsageInfo) {
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
fn persist_oauth_token(name: &str, oauth: &OAuthToken) -> Result<()> {
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

/// Default scheduler tick. `spawn_refresher` wakes every second and only
/// performs network work for profiles whose per-profile interval has elapsed.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Baseline refresh interval. Used as the default when no learned value exists
/// and as the quiet-period reset target.
pub(crate) const NORMAL_INTERVAL_MS: u64 = 30_000;

/// AIMD learner bounds and step. The learned value is clamped to [FLOOR, CEILING]
/// after every bump; STEP is the additive decrease per recovery round.
pub(crate) const LEARNED_FLOOR_MS: u64 = 10_000;
pub(crate) const LEARNED_CEILING_MS: u64 = 300_000;
pub(crate) const LEARNED_STEP_MS: u64 = 5_000;

/// After this many ms without a 429, a raised learned interval resets to
/// NORMAL_INTERVAL_MS so a single spike doesn't permanently raise the floor.
pub(crate) const LEARNED_QUIET_RESET_MS: u64 = 5 * 60 * 1_000;

/// Default fallback threshold (must match `fallback::DEFAULT_THRESHOLD`).
const DEFAULT_FALLBACK_THRESHOLD: f64 = 95.0;

/// Distance below the fallback threshold at which the refresher clamps to
/// LEARNED_FLOOR_MS regardless of the learned value.
const NEAR_THRESHOLD_MARGIN: f64 = 5.0;

/// Tolerance for "same five-hour utilization" comparison when deciding a
/// Fresh response is actually a server-side cache hit. Well below display
/// precision but above any plausible f64 round-trip jitter.
const CACHE_HIT_EPSILON: f64 = 1e-9;

/// Estimated TTL of the Anthropic `/usage` server-side cache. Two consecutive
/// Fresh responses with identical five-hour utilization only count as a
/// "server cache hit" when the second one landed within this window — beyond
/// it, an unchanged value just means the user isn't burning tokens, and that
/// must not pull the learner back up toward NORMAL.
const SERVER_CACHE_TTL_ESTIMATE_MS: u64 = 25_000;

pub(crate) type UsageStore = Arc<Mutex<HashMap<String, UsageInfo>>>;
pub(crate) type StatusStore = Arc<Mutex<HashMap<String, FetchStatus>>>;
pub(crate) type TokenList = Arc<Mutex<Vec<TokenEntry>>>;

/// Per-profile epoch-ms of the last fetch attempt (cache-rule gating).
pub(crate) type LastFetchedAt = Arc<Mutex<HashMap<String, u64>>>;

/// Names pushed here after a successful token rotation are fetched on the very
/// next scheduler tick, bypassing the per-profile cadence.
pub(crate) type RefetchQueue = Arc<Mutex<HashSet<String>>>;

/// Per-profile learned refresh interval in ms (AIMD cadence).
pub(crate) type LearnedIntervals = Arc<Mutex<HashMap<String, u64>>>;

/// How many consecutive non-429 fetches each profile has seen since the last backoff.
pub(crate) type ConsecutiveOk = Arc<Mutex<HashMap<String, u32>>>;

/// How many consecutive Fresh fetches with unchanged utilization each profile
/// has seen. Used to detect server-side cache hits and back off when polling
/// faster than the server invalidates. In-memory only; not persisted.
pub(crate) type ConsecutiveCacheHit = Arc<Mutex<HashMap<String, u32>>>;

/// Epoch-ms of the most recent 429 per profile. Used for quiet-period resets.
pub(crate) type Last429At = Arc<Mutex<HashMap<String, u64>>>;

/// Profiles that need an auto-start kick after the fetch revealed no live 5h
/// window. Main thread drains this set on every tick.
pub(crate) type PendingAutoStart = Arc<Mutex<HashSet<String>>>;

/// Profiles whose 5h window has just expired and need a token rotation.
/// Value: the `resets_at` epoch-secs pinned at detection time so the drain
/// stamps `LastRotatedWindow` with the exact window it acted on, not whatever
/// the store holds when the drain runs (which may already be a newer window).
pub(crate) type PendingWindowRotation = Arc<Mutex<HashMap<String, i64>>>;

/// Per-profile `resets_at` epoch-secs we already rotated on, so each expiry
/// fires exactly once.
pub(crate) type LastRotatedWindow = Arc<Mutex<HashMap<String, i64>>>;

/// Snapshot of one profile's OAuth identity used by the refresher.
#[derive(Clone)]
pub(crate) struct TokenEntry {
    pub(crate) name: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) fallback_threshold: f64,
    pub(crate) auto_start: bool,
}

/// Per-profile epoch-ms of the next scheduled fetch. Written by the scheduler
/// after each `partition_due` run so the overview rows can show a countdown
/// without re-running the partition math on the render thread.
pub(crate) type NextRefreshPerProfile = Arc<Mutex<HashMap<String, u64>>>;

/// Profile names currently being fetched. The overview row shows a busy pip
/// in the timer slot instead of a countdown while a fetch is in flight.
pub(crate) type FetchingNow = Arc<Mutex<HashSet<String>>>;

/// Outcome of the most recent usage fetch attempt for a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchStatus {
    /// Live response from the Anthropic API this tick.
    Fresh,
    /// API request failed; the numbers shown come from the on-disk cache.
    Cached,
    /// API request failed and no cache was available — no data to show.
    Failed,
    /// The API returned 429 on the initial call (the signal was observed this
    /// tick regardless of whether the retry after token rotation succeeded).
    /// The AIMD learner uses this to bump the per-profile interval up.
    RateLimited,
}

/// New access + refresh token returned by a successful in-fetch rotation.
/// The scheduler propagates this back into the live `TokenList` so the next
/// tick uses the rotated pair instead of re-401'ing with the stale access
/// token and burning the refresh-token chain by rotating again.
type RotatedTokens = (String, Option<String>);

/// One profile's fetch + rotate + retry path. On 401/429 we refresh the OAuth
/// pair, persist it, and retry once. A 429 on the initial call sets
/// `RateLimited` so the AIMD learner can back off; a successful retry still
/// records it because the rate-limit signal was observed this tick. Any other
/// error falls back to the on-disk cache. Pushes `name` onto `refetch` when
/// rotation succeeds but the follow-up fetch failed, so the next scheduler
/// tick re-fetches with the new token. Returns the rotated pair on success
/// so the caller can update the live `TokenList`.
fn fetch_with_rotation(
    name: &str,
    access_token: &str,
    refresh_token: Option<&str>,
    refetch: &RefetchQueue,
) -> (Option<UsageInfo>, FetchStatus, Option<RotatedTokens>) {
    let saw_429 = match fetch_raw(access_token) {
        Ok(info) => return (Some(info), FetchStatus::Fresh, None),
        Err(FetchError::Status(429)) => true,
        Err(FetchError::Status(401)) => false,
        Err(_) => {
            let (info, status) = load_disk_cache(name, FetchStatus::Cached);
            return (info, status, None);
        }
    };

    let fallback_status = if saw_429 {
        FetchStatus::RateLimited
    } else {
        FetchStatus::Cached
    };

    let Some(rt) = refresh_token else {
        let (info, status) = load_disk_cache(name, fallback_status);
        return (info, status, None);
    };
    let tok = match crate::oauth::refresh(rt) {
        Ok(t) => t,
        Err(_) => {
            let (info, status) = load_disk_cache(name, fallback_status);
            return (info, status, None);
        }
    };
    let oauth = OAuthToken {
        access_token: tok.access_token.clone(),
        refresh_token: Some(tok.refresh_token.clone()),
        expires_at: Some((now_ms() + tok.expires_in * 1000) as i64),
        scopes: tok
            .scope
            .as_ref()
            .map(|s| s.split_whitespace().map(String::from).collect()),
        subscription_type: None,
    };
    // Don't claim the rotation if we couldn't persist — the new tokens would
    // be lost on restart, and the caller would update the live list with a
    // pair the disk doesn't reflect.
    if persist_oauth_token(name, &oauth).is_err() {
        let (info, status) = load_disk_cache(name, fallback_status);
        return (info, status, None);
    }
    let rotated: Option<RotatedTokens> =
        Some((tok.access_token.clone(), Some(tok.refresh_token.clone())));
    match fetch_raw(&tok.access_token) {
        Ok(info) => {
            // Token rotated and fresh numbers are in hand. A 429 was still
            // observed this tick, so report RateLimited so the learner backs
            // off even though we recovered.
            let status = if saw_429 {
                FetchStatus::RateLimited
            } else {
                FetchStatus::Fresh
            };
            (Some(info), status, rotated)
        }
        Err(_) => {
            // Rotation succeeded but the follow-up fetch failed; schedule a
            // re-fetch so the next tick picks up with the new access token.
            if let Ok(mut q) = refetch.lock() {
                q.insert(name.to_string());
            }
            let (info, status) = load_disk_cache(name, fallback_status);
            (info, status, rotated)
        }
    }
}

fn five_hour_utilization(info: &UsageInfo) -> Option<f64> {
    info.five_hour.as_ref().map(|w| w.utilization)
}

fn five_hour_has_window(info: &UsageInfo) -> bool {
    info.five_hour
        .as_ref()
        .and_then(|w| w.resets_at.as_ref())
        .is_some()
}

/// Process-wide counter mixed into the bump-up jitter seed so two profiles
/// bumping in the same tick get distinct jitter — `subsec_nanos` alone often
/// collides under burst load, defeating the whole point of jittering.
static JITTER_COUNTER: AtomicU64 = AtomicU64::new(0);

fn jitter_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = JITTER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed = nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    mixed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407)
}

/// Multiplicative-increase: multiply current by 1.5 and add ±10% jitter,
/// clamped to ceiling. At saturation the result pins to CEILING with no
/// jitter — applying jitter after the clamp would let continued 429s drift
/// the effective ceiling down by half the jitter range.
pub(crate) fn bump_up(current: u64) -> u64 {
    let raised = current.saturating_mul(3) / 2;
    if raised >= LEARNED_CEILING_MS {
        return LEARNED_CEILING_MS;
    }
    let margin = raised / 10;
    let jitter = if margin == 0 {
        0i64
    } else {
        (jitter_seed() % (margin * 2 + 1)) as i64 - margin as i64
    };
    ((raised as i64 + jitter).max(LEARNED_FLOOR_MS as i64) as u64).min(LEARNED_CEILING_MS)
}

/// Additive-decrease: subtract LEARNED_STEP_MS, clamp to floor.
pub(crate) fn bump_down(current: u64) -> u64 {
    current
        .saturating_sub(LEARNED_STEP_MS)
        .max(LEARNED_FLOOR_MS)
}

/// True when an unchanged five-hour utilization should be attributed to the
/// server-side /usage cache rather than to genuine idle. Three conditions:
/// status must be Fresh (other statuses can't carry a "same as last" signal);
/// the poll must have landed inside the server cache window (slower polls
/// would see invalidated cache, so unchanged values mean the user just wasn't
/// burning tokens); both prev and new utilizations must be present and equal
/// within `CACHE_HIT_EPSILON`.
fn detect_cache_hit(
    status: FetchStatus,
    elapsed_ms: u64,
    prev_util: Option<f64>,
    new_util: Option<f64>,
) -> bool {
    if !matches!(status, FetchStatus::Fresh) {
        return false;
    }
    if elapsed_ms >= SERVER_CACHE_TTL_ESTIMATE_MS {
        return false;
    }
    match (prev_util, new_util) {
        (Some(a), Some(b)) => (a - b).abs() < CACHE_HIT_EPSILON,
        _ => false,
    }
}

/// Resolve the effective refresh interval for one profile. Near-threshold always
/// wins with FLOOR (highest urgency). Otherwise returns the learned interval,
/// defaulting to NORMAL when no learned value exists.
fn interval_for(
    entry: &TokenEntry,
    last_5h: Option<f64>,
    learned_intervals: &LearnedIntervals,
) -> u64 {
    let near = matches!(last_5h, Some(u) if u >= entry.fallback_threshold - NEAR_THRESHOLD_MARGIN);
    if near {
        return LEARNED_FLOOR_MS;
    }
    learned_intervals
        .lock()
        .ok()
        .and_then(|m| m.get(&entry.name).copied())
        .unwrap_or(NORMAL_INTERVAL_MS)
}

/// Update the AIMD learner maps for one profile based on the fetch outcome.
/// Called from the scheduler thread; all four maps are shared with the main
/// thread via `Arc<Mutex<...>>` and persisted to `AppState` on shutdown
/// (except `cache_hit_count`, which is in-memory only).
///
/// `cache_hit` is true when a Fresh response carried the same five-hour
/// utilization as the previously stored value — the Anthropic usage API has a
/// ~30s server-side cache, so unchanged numbers at FLOOR (10s) mean we're
/// polling faster than the server invalidates, not that the API is healthy.
fn update_learner(
    name: &str,
    status: FetchStatus,
    cache_hit: bool,
    learned: &LearnedIntervals,
    ok_count: &ConsecutiveOk,
    cache_hit_count: &ConsecutiveCacheHit,
    last_429: &Last429At,
) {
    let now = now_ms();

    let (Ok(mut learned_g), Ok(mut ok_g), Ok(mut ch_g), Ok(mut l429_g)) = (
        learned.lock(),
        ok_count.lock(),
        cache_hit_count.lock(),
        last_429.lock(),
    ) else {
        return;
    };

    // Quiet-period reset only fires on a confirmed Fresh: a 5-minute network
    // outage shouldn't undo legitimate backoff from prior 429s. Always remove
    // the stale 429 stamp when the quiet window has elapsed, even if the
    // learner is already at or below NORMAL — otherwise the entry lingers
    // forever on disk.
    if matches!(status, FetchStatus::Fresh)
        && let Some(&t429) = l429_g.get(name)
        && now.saturating_sub(t429) >= LEARNED_QUIET_RESET_MS
    {
        let current = learned_g.get(name).copied().unwrap_or(NORMAL_INTERVAL_MS);
        if current > NORMAL_INTERVAL_MS {
            learned_g.insert(name.to_string(), NORMAL_INTERVAL_MS);
        }
        l429_g.remove(name);
    }

    match status {
        FetchStatus::RateLimited => {
            let current = learned_g.get(name).copied().unwrap_or(NORMAL_INTERVAL_MS);
            learned_g.insert(name.to_string(), bump_up(current));
            ok_g.insert(name.to_string(), 0);
            ch_g.insert(name.to_string(), 0);
            l429_g.insert(name.to_string(), now);
        }
        // A Fresh response with the same utilization is a server-side cache hit:
        // back off additively toward NORMAL so we don't spin at FLOOR forever.
        FetchStatus::Fresh if cache_hit => {
            let hits = ch_g.get(name).copied().unwrap_or(0) + 1;
            if hits >= 2 {
                let current = learned_g.get(name).copied().unwrap_or(NORMAL_INTERVAL_MS);
                // Ceiling is NORMAL, not LEARNED_CEILING — cache hits mean "fine,
                // just polling too fast"; drift back to the baseline, not max backoff.
                let bumped = current
                    .saturating_add(LEARNED_STEP_MS)
                    .min(NORMAL_INTERVAL_MS);
                learned_g.insert(name.to_string(), bumped);
                ch_g.insert(name.to_string(), 0);
                ok_g.insert(name.to_string(), 0);
            } else {
                ch_g.insert(name.to_string(), hits);
            }
        }
        // Only a confirmed API 200 with changed data counts as a recovery signal.
        FetchStatus::Fresh => {
            let count = ok_g.get(name).copied().unwrap_or(0) + 1;
            if count >= 2 {
                let current = learned_g.get(name).copied().unwrap_or(NORMAL_INTERVAL_MS);
                learned_g.insert(name.to_string(), bump_down(current));
                ok_g.insert(name.to_string(), 0);
            } else {
                ok_g.insert(name.to_string(), count);
            }
            ch_g.insert(name.to_string(), 0);
        }
        // Network failures and cache fallbacks neither confirm nor refute API
        // health — leave the counters alone so a single blip doesn't wipe
        // legitimate recovery progress accumulated from prior Fresh responses.
        FetchStatus::Cached | FetchStatus::Failed => {}
    }
}

/// Outcome of one profile's fetch step inside the scheduler tick. Holds the
/// data the scheduler needs to update shared state on the main loop side of
/// the spawned thread.
struct FetchOutcome {
    name: String,
    info: Option<UsageInfo>,
    status: FetchStatus,
    needs_auto_start: bool,
    /// New `(access_token, refresh_token)` pair when the fetch path rotated
    /// the OAuth tokens. The scheduler propagates this into the live
    /// `TokenList` so the next tick uses the fresh pair.
    rotated: Option<RotatedTokens>,
}

/// Run a single fetch for one entry.
fn run_fetch(entry: TokenEntry, refetch: &RefetchQueue) -> FetchOutcome {
    let (info, status, rotated) = fetch_with_rotation(
        &entry.name,
        &entry.access_token,
        entry.refresh_token.as_deref(),
        refetch,
    );

    let needs_auto_start = match info.as_ref() {
        Some(i) => entry.auto_start && !five_hour_has_window(i),
        None => false,
    };

    FetchOutcome {
        name: entry.name,
        info,
        status,
        needs_auto_start,
        rotated,
    }
}

/// Write one outcome into the shared stores. Disk cache is updated on every
/// successful API response so a later process restart can still surface
/// numbers without a fresh API call.
#[allow(clippy::too_many_arguments)]
fn apply_outcome(
    outcome: FetchOutcome,
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    pending_auto_start: &PendingAutoStart,
    learned: &LearnedIntervals,
    ok_count: &ConsecutiveOk,
    cache_hit_count: &ConsecutiveCacheHit,
    last_429: &Last429At,
) {
    // Capture previous five-hour utilization before overwriting the store so
    // we can tell whether this Fresh response is a server-side cache hit.
    let prev_util: Option<f64> = store
        .lock()
        .ok()
        .and_then(|s| s.get(&outcome.name).and_then(five_hour_utilization));

    if let Some(info) = &outcome.info {
        let is_fresh = matches!(
            outcome.status,
            FetchStatus::Fresh | FetchStatus::RateLimited
        );
        if is_fresh {
            write_disk_cache(&outcome.name, info);
        }
        if let Ok(mut s) = store.lock() {
            // Don't clobber newer Fresh data with older Cached snapshots loaded
            // from disk by `fetch_with_rotation`'s fallback path. Cached only
            // fills the store when no entry exists (cold start without network).
            if is_fresh || !s.contains_key(&outcome.name) {
                s.insert(outcome.name.clone(), info.clone());
            }
        }
    }

    // Elapsed since the previous fetch attempt for this profile. Read before
    // we overwrite `last_fetched` below. Used to distinguish a true server-side
    // cache hit (poll landed inside the ~30s cache window with unchanged
    // numbers) from an idle period (poll at NORMAL pace, user just isn't
    // burning tokens). Without this gate, idle periods misclassify as cache
    // hits and the AIMD learner gets dragged back up to NORMAL on every pause.
    let elapsed_ms: u64 = last_fetched
        .lock()
        .ok()
        .and_then(|m| m.get(&outcome.name).copied())
        .map(|prev| now_ms().saturating_sub(prev))
        .unwrap_or(u64::MAX);

    let new_util: Option<f64> = outcome.info.as_ref().and_then(five_hour_utilization);
    let cache_hit = detect_cache_hit(outcome.status, elapsed_ms, prev_util, new_util);

    if let Ok(mut st) = status.lock() {
        st.insert(outcome.name.clone(), outcome.status);
    }
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(outcome.name.clone(), now_ms());
    }
    if outcome.needs_auto_start
        && let Ok(mut p) = pending_auto_start.lock()
    {
        p.insert(outcome.name.clone());
    }
    update_learner(
        &outcome.name,
        outcome.status,
        cache_hit,
        learned,
        ok_count,
        cache_hit_count,
        last_429,
    );
}

/// Force-fetch every entry right now in parallel and write the results into
/// the shared stores. Bypasses the cache rule — used by `bootstrap_usage`
/// and `manual_refresh`. Blocks until all fetches complete. One-shot, so any
/// `rotated` tokens are dropped — the main thread's `reload_if_state_changed`
/// will pick them up from the persisted `credentials.json` shortly.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_all_into(
    tokens: &[TokenEntry],
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    pending_auto_start: &PendingAutoStart,
    refetch: &RefetchQueue,
    learned: &LearnedIntervals,
    ok_count: &ConsecutiveOk,
    cache_hit_count: &ConsecutiveCacheHit,
    last_429: &Last429At,
) {
    if tokens.is_empty() {
        return;
    }

    let handles: Vec<_> = tokens
        .iter()
        .cloned()
        .map(|entry| {
            let refetch = Arc::clone(refetch);
            std::thread::spawn(move || run_fetch(entry, &refetch))
        })
        .collect();

    for h in handles {
        let Ok(outcome) = h.join() else {
            continue;
        };
        apply_outcome(
            outcome,
            store,
            status,
            last_fetched,
            pending_auto_start,
            learned,
            ok_count,
            cache_hit_count,
            last_429,
        );
    }
}

/// Background scheduler. Wakes every second and fans out parallel fetches for
/// every profile whose per-profile interval has elapsed. The effective interval
/// per profile comes from the AIMD learner (stored in `learned`): FLOOR when
/// near the configured fallback threshold, the learned value otherwise, falling
/// back to NORMAL when no learned value exists.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_refresher(
    tokens: TokenList,
    store: UsageStore,
    status: StatusStore,
    next_refresh_per_profile: NextRefreshPerProfile,
    fetching_now: FetchingNow,
    last_fetched: LastFetchedAt,
    pending_auto_start: PendingAutoStart,
    pending_window_rotation: PendingWindowRotation,
    last_rotated_window: LastRotatedWindow,
    refetch_queue: RefetchQueue,
    learned: LearnedIntervals,
    ok_count: ConsecutiveOk,
    cache_hit_count: ConsecutiveCacheHit,
    last_429: Last429At,
) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(TICK_INTERVAL);

            let snapshot: Vec<TokenEntry> = match tokens.lock() {
                Ok(t) => t.clone(),
                Err(_) => continue,
            };
            if snapshot.is_empty() {
                continue;
            }

            // Drain names pushed by rotation paths so they bypass the cadence
            // and get fresh numbers on this tick instead of waiting up to 45s.
            let forced: HashSet<String> = refetch_queue
                .lock()
                .ok()
                .map(|mut q| std::mem::take(&mut *q))
                .unwrap_or_default();

            // Decide which entries are due this tick. Per-profile intervals
            // come from the AIMD learner held in `learned`.
            let now = now_ms();
            let (mut due, _soonest_next, mut per_profile_next) =
                partition_due(&snapshot, now, &store, &last_fetched, &learned);

            // Merge forced entries that aren't already scheduled this tick and
            // reflect them in the published map as "due now" (zero countdown).
            if !forced.is_empty() {
                let mut extras: Vec<TokenEntry> = Vec::with_capacity(forced.len());
                for entry in snapshot
                    .iter()
                    .filter(|e| forced.contains(&e.name) && !due.iter().any(|d| d.name == e.name))
                {
                    per_profile_next.insert(entry.name.clone(), now);
                    extras.push(entry.clone());
                }
                due.extend(extras);
            }

            // Publish the per-profile next times AFTER the forced merge so the
            // UI countdown doesn't show "in Xs" for a profile that is in fact
            // fetching this very tick.
            if let Ok(mut nrpp) = next_refresh_per_profile.lock() {
                nrpp.clone_from(&per_profile_next);
            }

            if due.is_empty() {
                continue;
            }

            // Mark profiles as in-flight so the overview row shows a pip.
            if let Ok(mut fn_set) = fetching_now.lock() {
                for entry in &due {
                    fn_set.insert(entry.name.clone());
                }
            }

            let handles: Vec<_> = due
                .into_iter()
                .map(|entry| {
                    let refetch_queue = Arc::clone(&refetch_queue);
                    std::thread::spawn(move || run_fetch(entry, &refetch_queue))
                })
                .collect();
            for h in handles {
                let Ok(outcome) = h.join() else {
                    continue;
                };
                // Clear the in-flight marker before writing results so the
                // overview row transitions from pip → fresh countdown atomically
                // from the render thread's perspective (it reads both under separate
                // locks, but a brief flicker to "no pip + stale timer" is acceptable).
                if let Ok(mut fn_set) = fetching_now.lock() {
                    fn_set.remove(&outcome.name);
                }
                // Propagate any rotated OAuth pair back into the live snapshot
                // before the next tick — otherwise tick N+1 reuses the stale
                // access token, 401s, rotates again, and burns the refresh-token
                // chain while waiting for the mtime watch to reload AppConfig.
                if let Some((new_access, new_refresh)) = &outcome.rotated
                    && let Ok(mut t) = tokens.lock()
                    && let Some(entry) = t.iter_mut().find(|e| e.name == outcome.name)
                {
                    entry.access_token = new_access.clone();
                    entry.refresh_token = new_refresh.clone();
                }
                apply_outcome(
                    outcome,
                    &store,
                    &status,
                    &last_fetched,
                    &pending_auto_start,
                    &learned,
                    &ok_count,
                    &cache_hit_count,
                    &last_429,
                );
            }

            // After fetches complete, check for profiles whose 5h window has
            // expired (now >= resets_at + 1s) and haven't been rotated for
            // that window yet. Post them to the main thread's drain queue —
            // avoids holding &mut AppConfig in the scheduler thread.
            scan_expired_windows(
                &snapshot,
                &store,
                &last_rotated_window,
                &pending_window_rotation,
            );

            // Recompute per-profile next times AFTER fetches have updated
            // `last_fetched` so the overview countdowns reflect fresh deadlines.
            let (_, _, per_profile_after) =
                partition_due(&snapshot, now_ms(), &store, &last_fetched, &learned);
            if let Ok(mut nrpp) = next_refresh_per_profile.lock() {
                nrpp.clone_from(&per_profile_after);
            }
        }
    });
}

/// For each profile in `snapshot`, check whether its 5h window has expired
/// (current time is at least 1s past `resets_at`) and we haven't already
/// queued a rotation for that specific `resets_at` epoch. Qualifying profiles
/// are pushed into `pending_window_rotation` for the main thread to drain.
fn scan_expired_windows(
    snapshot: &[TokenEntry],
    store: &UsageStore,
    last_rotated_window: &LastRotatedWindow,
    pending: &PendingWindowRotation,
) {
    let now = now_epoch_secs();
    let st = store.lock().ok();
    let lrw = last_rotated_window.lock().ok();
    let pend = pending.lock().ok();

    let (Some(st), Some(lrw), Some(ref mut pend)) = (st, lrw, pend) else {
        return;
    };

    for entry in snapshot {
        let Some(resets_at_str) = st
            .get(&entry.name)
            .and_then(|u| u.five_hour.as_ref())
            .and_then(|w| w.resets_at.as_deref())
        else {
            continue;
        };
        let Some(resets_at) = iso_to_epoch_secs(resets_at_str) else {
            continue;
        };
        // 1s past the window boundary to avoid acting on a window that hasn't
        // fully closed yet.
        if now < resets_at + 1 {
            continue;
        }
        // Already acted on this specific window.
        if lrw.get(&entry.name).copied().unwrap_or(0) == resets_at {
            continue;
        }
        // Pin the epoch at detection time. The drain uses this value to stamp
        // `LastRotatedWindow` so it deduplicates the window it actually saw,
        // not a potentially newer one the store holds by the time the drain runs.
        pend.insert(entry.name.clone(), resets_at);
    }
}

/// Split `snapshot` into the subset due this tick, the soonest epoch-ms at
/// which any non-due entry will become due, and a per-profile map of next
/// fetch epoch-ms. A poisoned `last_fetched` returns empty rather than
/// falling back to `last=0` (which would mark every profile due → fetch storm).
fn partition_due(
    snapshot: &[TokenEntry],
    now: u64,
    store: &UsageStore,
    last_fetched: &LastFetchedAt,
    learned: &LearnedIntervals,
) -> (Vec<TokenEntry>, u64, HashMap<String, u64>) {
    let Ok(lf) = last_fetched.lock() else {
        return (Vec::new(), now + NORMAL_INTERVAL_MS, HashMap::new());
    };
    let st = store.lock().ok();

    let mut due = Vec::new();
    let mut soonest_next = now + NORMAL_INTERVAL_MS;
    let mut per_profile = HashMap::with_capacity(snapshot.len());
    for entry in snapshot {
        let last = lf.get(&entry.name).copied().unwrap_or(0);
        let last_5h = st
            .as_ref()
            .and_then(|s| s.get(&entry.name).and_then(five_hour_utilization));
        let interval = interval_for(entry, last_5h, learned);
        let next = last.saturating_add(interval);
        per_profile.insert(entry.name.clone(), next);
        if now >= next {
            due.push(entry.clone());
        } else if next < soonest_next {
            soonest_next = next;
        }
    }
    (due, soonest_next, per_profile)
}

/// Default fallback threshold used when a profile leaves it unset. Public so
/// `App::collect_tokens` can resolve once at snapshot time instead of every
/// scheduler tick.
pub(crate) const fn default_fallback_threshold() -> f64 {
    DEFAULT_FALLBACK_THRESHOLD
}

#[cfg(test)]
#[path = "../tests/inline/learned_cadence.rs"]
mod tests;
