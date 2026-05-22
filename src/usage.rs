use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::lock::with_state_lock;
use crate::profile::{ClaudeCredentials, OAuthToken, atomic_write, clauth_dir, profile_dir};

const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/profile";

/// Default scheduler tick. `spawn_refresher` wakes every second and only
/// performs network work for profiles whose per-profile interval has elapsed.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Per-profile refresh cadences. Near-threshold profiles get the hot interval;
/// stable profiles (utilization unchanged since the last fetch) get the cold
/// one to amortize API pressure. Default profiles use `NORMAL_INTERVAL_MS`.
const HOT_INTERVAL_MS: u64 = 15_000;
const NORMAL_INTERVAL_MS: u64 = 30_000;
const COLD_INTERVAL_MS: u64 = 45_000;

/// Default fallback threshold (must match `fallback::DEFAULT_THRESHOLD`).
const DEFAULT_FALLBACK_THRESHOLD: f64 = 95.0;

/// Distance below the fallback threshold at which the refresher swaps to the
/// hot interval and ignores the cache rule.
const NEAR_THRESHOLD_MARGIN: f64 = 5.0;

pub(crate) type UsageStore = Arc<Mutex<HashMap<String, UsageInfo>>>;
pub(crate) type StatusStore = Arc<Mutex<HashMap<String, FetchStatus>>>;
pub(crate) type TokenList = Arc<Mutex<Vec<TokenEntry>>>;

/// Per-profile epoch-ms of the last fetch attempt (cache-rule gating).
pub(crate) type LastFetchedAt = Arc<Mutex<HashMap<String, u64>>>;

/// Per-profile flag: previous fetch's 5h utilization matched the one before
/// it. Powers the cold-cache 45s cadence.
pub(crate) type LastStable = Arc<Mutex<HashMap<String, bool>>>;

/// Profiles that need an auto-start kick after the fetch revealed no live 5h
/// window. Main thread drains this set on every tick.
pub(crate) type PendingAutoStart = Arc<Mutex<HashSet<String>>>;

/// Snapshot of one profile's OAuth identity used by the refresher.
#[derive(Clone)]
pub(crate) struct TokenEntry {
    pub(crate) name: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) fallback_threshold: f64,
    pub(crate) auto_start: bool,
}

/// Coarse shared signal observed by the TUI header. Set true while any
/// background fetch is in flight, so the Claude logo can flash sapphire.
pub(crate) type ActivityFlag = Arc<AtomicBool>;

/// Epoch-ms of the next scheduled refresh tick. Powers the footer countdown.
pub(crate) type NextRefreshAt = Arc<AtomicU64>;

/// Outcome of the most recent usage fetch attempt for a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchStatus {
    /// Live response from the Anthropic API this tick.
    Fresh,
    /// API request failed; the numbers shown come from the on-disk cache.
    Cached,
    /// API request failed and no cache was available — no data to show.
    Failed,
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

/// Read the on-disk usage cache for `name`. Returns `Cached` when a snapshot
/// is available, `Failed` when no cache exists.
fn load_disk_cache(name: &str, status: FetchStatus) -> (Option<UsageInfo>, FetchStatus) {
    let cache = cache_path(name);
    match cache.and_then(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        serde_json::from_str::<UsageInfo>(&text).ok()
    }) {
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

/// One profile's fetch + rotate + retry path. On 401/429 we refresh the OAuth
/// pair, persist it, and retry once. Any other error path falls back to the
/// on-disk cache.
fn fetch_with_rotation(
    name: &str,
    access_token: &str,
    refresh_token: Option<&str>,
) -> (Option<UsageInfo>, FetchStatus) {
    match fetch_raw(access_token) {
        Ok(info) => return (Some(info), FetchStatus::Fresh),
        Err(FetchError::Status(401 | 429)) => {}
        Err(_) => return load_disk_cache(name, FetchStatus::Cached),
    }

    let Some(rt) = refresh_token else {
        return load_disk_cache(name, FetchStatus::Cached);
    };
    let tok = match crate::oauth::refresh(rt) {
        Ok(t) => t,
        Err(_) => return load_disk_cache(name, FetchStatus::Cached),
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
    if persist_oauth_token(name, &oauth).is_err() {
        return load_disk_cache(name, FetchStatus::Cached);
    }
    match fetch_raw(&tok.access_token) {
        Ok(info) => (Some(info), FetchStatus::Fresh),
        Err(_) => load_disk_cache(name, FetchStatus::Cached),
    }
}

fn cache_path(profile_name: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".clauth")
            .join("profiles")
            .join(profile_name)
            .join("usage_cache.json")
    })
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

/// Compute the per-profile refresh interval for one entry. Hot when within
/// `NEAR_THRESHOLD_MARGIN` of the configured fallback threshold; cold when
/// the previous fetch was stable; normal otherwise.
fn interval_for(entry: &TokenEntry, last_5h: Option<f64>, stable: bool) -> u64 {
    let near = matches!(last_5h, Some(u) if u >= entry.fallback_threshold - NEAR_THRESHOLD_MARGIN);
    if near {
        HOT_INTERVAL_MS
    } else if stable {
        COLD_INTERVAL_MS
    } else {
        NORMAL_INTERVAL_MS
    }
}

/// Outcome of one profile's fetch step inside the scheduler tick. Holds the
/// data the scheduler needs to update shared state on the main loop side of
/// the spawned thread.
struct FetchOutcome {
    name: String,
    info: Option<UsageInfo>,
    status: FetchStatus,
    stable: bool,
    needs_auto_start: bool,
}

/// Run a single fetch for one entry. Pulls the previous 5h utilization out
/// of the store before issuing the request so we can compute stability
/// without holding the lock across I/O.
fn run_fetch(entry: TokenEntry, store: &UsageStore) -> FetchOutcome {
    let prev_util = store
        .lock()
        .ok()
        .and_then(|s| s.get(&entry.name).and_then(five_hour_utilization));

    let (info, status) = fetch_with_rotation(
        &entry.name,
        &entry.access_token,
        entry.refresh_token.as_deref(),
    );

    let (stable, needs_auto_start) = match info.as_ref() {
        Some(i) => {
            let stable = match (prev_util, five_hour_utilization(i)) {
                (Some(a), Some(b)) => (a - b).abs() < f64::EPSILON,
                _ => false,
            };
            let needs_kick = entry.auto_start && !five_hour_has_window(i);
            (stable, needs_kick)
        }
        None => (false, false),
    };

    FetchOutcome {
        name: entry.name,
        info,
        status,
        stable,
        needs_auto_start,
    }
}

/// Write one outcome into the shared stores. Disk cache is updated on every
/// successful API response so a later process restart can still surface
/// numbers without a fresh API call.
fn apply_outcome(
    outcome: FetchOutcome,
    store: &UsageStore,
    status: &StatusStore,
    last_fetched: &LastFetchedAt,
    last_stable: &LastStable,
    pending_auto_start: &PendingAutoStart,
) {
    if let Some(info) = &outcome.info {
        if matches!(outcome.status, FetchStatus::Fresh) {
            write_disk_cache(&outcome.name, info);
        }
        if let Ok(mut s) = store.lock() {
            s.insert(outcome.name.clone(), info.clone());
        }
    }
    if let Ok(mut st) = status.lock() {
        st.insert(outcome.name.clone(), outcome.status);
    }
    if let Ok(mut lf) = last_fetched.lock() {
        lf.insert(outcome.name.clone(), now_ms());
    }
    if let Ok(mut ls) = last_stable.lock() {
        ls.insert(outcome.name.clone(), outcome.stable);
    }
    if outcome.needs_auto_start
        && let Ok(mut p) = pending_auto_start.lock()
    {
        p.insert(outcome.name);
    }
}

/// Force-fetch every entry right now in parallel and write the results into
/// the shared stores. Bypasses the cache rule — used by `bootstrap_usage`
/// and `manual_refresh`. Blocks until all fetches complete.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fetch_all_into(
    tokens: &[TokenEntry],
    store: &UsageStore,
    status: &StatusStore,
    activity: &ActivityFlag,
    last_fetched: &LastFetchedAt,
    last_stable: &LastStable,
    pending_auto_start: &PendingAutoStart,
) {
    if tokens.is_empty() {
        return;
    }
    activity.store(true, Ordering::Relaxed);

    let handles: Vec<_> = tokens
        .iter()
        .cloned()
        .map(|entry| {
            let store = Arc::clone(store);
            std::thread::spawn(move || run_fetch(entry, &store))
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
            last_stable,
            pending_auto_start,
        );
    }

    activity.store(false, Ordering::Relaxed);
}

/// Background scheduler. Wakes every second and fans out parallel fetches
/// for every profile whose per-profile interval has elapsed. Per-profile
/// cadence: 15s when near the configured fallback threshold, 45s when the
/// previous fetch was stable, 30s otherwise. The cache rule (skip when the
/// last fetch was <45s ago AND stable) is folded into the cadence: a stable
/// profile's interval IS the cache-min-age, so the next tick lines up with
/// the moment the cache rule would expire.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_refresher(
    tokens: TokenList,
    store: UsageStore,
    status: StatusStore,
    activity: ActivityFlag,
    next_at: NextRefreshAt,
    last_fetched: LastFetchedAt,
    last_stable: LastStable,
    pending_auto_start: PendingAutoStart,
) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(TICK_INTERVAL);

            let snapshot: Vec<TokenEntry> = match tokens.lock() {
                Ok(t) => t.clone(),
                Err(_) => continue,
            };
            if snapshot.is_empty() {
                next_at.store(now_ms() + NORMAL_INTERVAL_MS, Ordering::Relaxed);
                continue;
            }

            // Decide which entries are due this tick. Per-profile intervals
            // are computed against the previous fetch's stability and 5h
            // utilization, both held in the shared stores.
            let now = now_ms();
            let (due, soonest_next) =
                partition_due(&snapshot, now, &store, &last_fetched, &last_stable);

            if due.is_empty() {
                next_at.store(soonest_next, Ordering::Relaxed);
                continue;
            }

            activity.store(true, Ordering::Relaxed);
            let handles: Vec<_> = due
                .into_iter()
                .map(|entry| {
                    let store = Arc::clone(&store);
                    std::thread::spawn(move || run_fetch(entry, &store))
                })
                .collect();
            for h in handles {
                let Ok(outcome) = h.join() else {
                    continue;
                };
                apply_outcome(
                    outcome,
                    &store,
                    &status,
                    &last_fetched,
                    &last_stable,
                    &pending_auto_start,
                );
            }
            activity.store(false, Ordering::Relaxed);

            // Recompute the soonest next due moment AFTER fetches have
            // updated `last_fetched`, so the footer countdown reflects the
            // freshly-recorded deadlines instead of the pre-tick estimate.
            let (_, soonest_after) =
                partition_due(&snapshot, now_ms(), &store, &last_fetched, &last_stable);
            next_at.store(soonest_after, Ordering::Relaxed);
        }
    });
}

/// Split `snapshot` into the subset due this tick and the soonest epoch-ms
/// at which any non-due entry will become due. The empty-list case is
/// callers' responsibility (we never get here with an empty snapshot).
fn partition_due(
    snapshot: &[TokenEntry],
    now: u64,
    store: &UsageStore,
    last_fetched: &LastFetchedAt,
    last_stable: &LastStable,
) -> (Vec<TokenEntry>, u64) {
    let lf = last_fetched.lock().ok();
    let ls = last_stable.lock().ok();
    let st = store.lock().ok();

    let mut due = Vec::new();
    let mut soonest_next = now + NORMAL_INTERVAL_MS;
    for entry in snapshot {
        let last = lf
            .as_ref()
            .and_then(|m| m.get(&entry.name).copied())
            .unwrap_or(0);
        let stable = ls
            .as_ref()
            .and_then(|m| m.get(&entry.name).copied())
            .unwrap_or(false);
        let last_5h = st
            .as_ref()
            .and_then(|s| s.get(&entry.name).and_then(five_hour_utilization));
        let interval = interval_for(entry, last_5h, stable);
        let next = last.saturating_add(interval);
        if now >= next {
            due.push(entry.clone());
        } else if next < soonest_next {
            soonest_next = next;
        }
    }
    (due, soonest_next)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Default fallback threshold used when a profile leaves it unset. Public so
/// `App::collect_tokens` can resolve once at snapshot time instead of every
/// scheduler tick.
pub(crate) const fn default_fallback_threshold() -> f64 {
    DEFAULT_FALLBACK_THRESHOLD
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
