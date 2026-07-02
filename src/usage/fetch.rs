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

/// Minimum spacing between consecutive requests to the same endpoint host,
/// enforced process-wide and keyed per host (see [`NEXT_REQUEST_SLOT`]). Accounts
/// sharing a host (every Anthropic OAuth account hits `api.anthropic.com`) pace this
/// far apart so a same-instant multi-profile burst (startup, refetch-queue drains, a
/// window-reset kick fan-out) can't trip a 429; accounts on distinct hosts (each
/// api-key provider) reserve independent slots and never wait on each other. Steady
/// polling sits well below this rate, so it only bites on bursts.
const REQUEST_SPACING_MS: u64 = 5_000;

/// Origin all OAuth `/usage`, `/profile`, and `/v1/messages` kick requests target —
/// they are hardcoded to this host regardless of a profile's `base_url`, so it is
/// their per-host pacing key in [`NEXT_REQUEST_SLOT`].
pub(crate) const ANTHROPIC_ORIGIN: &str = "https://api.anthropic.com";

/// Earliest epoch-ms the next request to each host may fire, keyed by endpoint
/// origin. Each caller reserves its host's next free slot (advancing it by
/// [`REQUEST_SPACING_MS`]) and sleeps until then. Leaf-ranked and held only to
/// reserve the slot — never across the sleep or the HTTP round trip.
static NEXT_REQUEST_SLOT: LazyLock<RankedMutex<HashMap<String, u64>, rank::UsageThrottle>> =
    LazyLock::new(|| RankedMutex::new(HashMap::new()));

/// Pure slot reservation: from a host's current earliest-allowed slot and `now`,
/// return `(advanced_slot, wait_ms)` — the slot reserved for the next caller on that
/// host (one [`REQUEST_SPACING_MS`] past this caller's fire time) and how long this
/// caller must wait for its own slot.
fn reserve_slot(current_slot: u64, now: u64) -> (u64, u64) {
    let fire_at = current_slot.max(now);
    (
        fire_at.saturating_add(REQUEST_SPACING_MS),
        fire_at.saturating_sub(now),
    )
}

/// Block until this caller's spacing slot for `host`, reserving the following slot
/// for the next caller on the same host. Distinct hosts hold independent slots, so
/// requests to different endpoints never serialize against each other. A poisoned
/// lock skips throttling rather than stalling the fetch.
pub(crate) fn await_request_slot(host: &str) {
    let now = now_ms();
    let wait_ms = {
        let Ok(mut slots) = NEXT_REQUEST_SLOT.lock() else {
            return;
        };
        let slot = slots.entry(host.to_string()).or_insert(0);
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
    /// Per-period credit breakdowns (`daily`/`weekly`) — shape is not yet
    /// observable on any account, so they're held as raw JSON and read
    /// defensively at render time (see [`ExtraPeriod::from_value`]); a number,
    /// object, or null all parse without breaking the `/usage` body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) daily: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) weekly: Option<serde_json::Value>,
}

/// A defensively-extracted view of an `extra_usage.daily`/`weekly` sub-object.
/// The real shape is undocumented and null on every reachable account, so this
/// pulls only the numeric fields it recognizes and treats anything else as
/// absent — never a parse failure.
#[derive(Debug, Clone, Default)]
pub(crate) struct ExtraPeriod {
    pub(crate) used_credits: Option<f64>,
    pub(crate) utilization: Option<f64>,
    pub(crate) monthly_limit: Option<f64>,
    pub(crate) currency: Option<String>,
}

impl ExtraPeriod {
    /// Pull the recognized numeric fields out of a raw `daily`/`weekly` value.
    /// `None` when the value carries nothing renderable.
    pub(crate) fn from_value(v: &serde_json::Value) -> Option<Self> {
        let obj = v.as_object()?;
        let num = |k: &str| obj.get(k).and_then(serde_json::Value::as_f64);
        let p = ExtraPeriod {
            used_credits: num("used_credits"),
            utilization: num("utilization"),
            monthly_limit: num("monthly_limit"),
            currency: obj
                .get("currency")
                .and_then(|c| c.as_str())
                .map(str::to_string),
        };
        (p.utilization.is_some() || p.used_credits.is_some()).then_some(p)
    }
}

/// Absolute used/limit dollar figures for a usage window, from the raw
/// `used_dollars`/`limit_dollars` fields. The wire shape is undetermined (Claude
/// Code's own client ignores these), so they're parsed leniently (see
/// [`json_to_dollars`]) and only surface when a value is actually present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WindowDollars {
    pub(crate) label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<f64>,
}

/// Lenient money → dollars. Accepts a bare number (already dollars, per the
/// `_dollars` field name), a `{amount_minor, exponent}` minor-unit object, or an
/// `{amount}` object (number or numeric string). Anything else → `None`, never a
/// panic or parse error — the wire shape is unconfirmed.
fn json_to_dollars(v: &serde_json::Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    let obj = v.as_object()?;
    if let Some(minor) = obj.get("amount_minor").and_then(serde_json::Value::as_i64) {
        let exp = obj
            .get("exponent")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(2) as i32;
        return Some(minor as f64 / 10f64.powi(exp));
    }
    match obj.get("amount") {
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// A per-model weekly window derived from a `weekly_scoped` entry in the
/// `/usage` `limits[]` array. `label` is built from the scope's model name
/// (`"7d fable"`, `"7d opus"`, …), so a model the server adds later shows up as
/// a bar with no code change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ScopedWindow {
    pub(crate) label: String,
    #[serde(flatten)]
    pub(crate) window: UsageWindow,
}

/// Pay-as-you-go spend / credit cap from the `/usage` `spend` block. Distinct
/// from [`ExtraUsage`] (the legacy credits field): the API now returns both, so
/// each renders its own bar when populated. Dollar figures are normalized from
/// the API's minor-unit money objects.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct SpendInfo {
    #[serde(default)]
    pub(crate) enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) currency: Option<String>,
}

impl SpendInfo {
    /// Build from the raw `spend` block, converting minor-unit money to dollars.
    fn from_raw(s: &RawSpend) -> Self {
        SpendInfo {
            enabled: s.enabled,
            used: s.used.as_ref().and_then(RawMoney::to_dollars),
            limit: s.limit.as_ref().and_then(RawMoney::to_dollars),
            percent: s.percent,
            currency: s
                .used
                .as_ref()
                .or(s.limit.as_ref())
                .and_then(|m| m.currency.clone()),
        }
    }

    /// A spend bar is worth showing once the account has a cap enabled or a
    /// limit set; disabled accounts (the current default) render nothing.
    pub(crate) fn is_visible(&self) -> bool {
        self.enabled || self.limit.is_some()
    }
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
    /// Per-model weekly windows (`weekly_scoped` limits) in `limits[]` order —
    /// grows as the server exposes new models, no per-model field needed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) weekly_scoped: Vec<ScopedWindow>,
    /// Absolute dollar figures per window label (`5h`/`7d`), when the endpoint
    /// carries `used_dollars`/`limit_dollars`. Empty on every current account.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) window_dollars: Vec<WindowDollars>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) extra_usage: Option<ExtraUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spend: Option<SpendInfo>,
}

/// Fixed labels for the two always-present windows. Per-model weekly labels are
/// built dynamically from the scope name (see [`ScopedWindow`]).
pub(crate) const LABEL_5H: &str = "5h";
pub(crate) const LABEL_7D: &str = "7d";

impl UsageInfo {
    /// All available windows as `(label, &UsageWindow)` pairs: 5h, 7d, then each
    /// per-model weekly window in `limits[]` order.
    pub(crate) fn windows(&self) -> Vec<(&str, &UsageWindow)> {
        let mut out = Vec::new();
        if let Some(w) = &self.five_hour {
            out.push((LABEL_5H, w));
        }
        if let Some(w) = &self.seven_day {
            out.push((LABEL_7D, w));
        }
        for s in &self.weekly_scoped {
            out.push((s.label.as_str(), &s.window));
        }
        out
    }

    /// Most representative weekly window: the aggregate `seven_day` when present,
    /// else the first per-model window.
    pub(crate) fn weekly_window(&self) -> Option<&UsageWindow> {
        self.seven_day
            .as_ref()
            .or_else(|| self.weekly_scoped.first().map(|s| &s.window))
    }
}

/// Nominal length of the rolling window named by `label`, in seconds. `None`
/// for labels with no fixed window (e.g. the monthly extra-credits bar).
pub(crate) fn window_duration_secs(label: &str) -> Option<i64> {
    if label == LABEL_5H {
        Some(5 * 3600)
    } else if label == LABEL_7D || label.starts_with("7d ") {
        // `7d` plus every per-model weekly label (`"7d fable"`, `"7d opus"`, …).
        Some(7 * 86_400)
    } else {
        // Provider window labels of the form `<n>h` / `<n>d` (e.g. z.ai's
        // `5h`/`7d`/`30d`) so any api-key account with a windowed limit gets the
        // same average pace + ideal-pace line as the OAuth windows.
        parse_nh_nd_label(label)
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
    // Legacy top-level windows — the fallback for when `limits[]` omits a
    // `session` / `weekly_all` entry, and the only carrier of the per-window
    // `*_dollars` figures (they never appear on `limits[]` entries).
    #[serde(default)]
    five_hour: Option<RawWindow>,
    #[serde(default)]
    seven_day: Option<RawWindow>,
    /// Normalized rate-limit list — the source of truth for every window.
    #[serde(default)]
    limits: Vec<RawLimit>,
    #[serde(default)]
    extra_usage: Option<ExtraUsage>,
    #[serde(default)]
    spend: Option<RawSpend>,
}

/// A top-level window object (`five_hour`/`seven_day`). Carries the percentage +
/// reset like [`UsageWindow`] plus the lenient `*_dollars` figures held as raw
/// JSON (undetermined shape).
#[derive(Deserialize)]
struct RawWindow {
    #[serde(default)]
    utilization: f64,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    used_dollars: Option<serde_json::Value>,
    #[serde(default)]
    limit_dollars: Option<serde_json::Value>,
}

impl RawWindow {
    fn to_window(&self) -> UsageWindow {
        UsageWindow {
            utilization: self.utilization,
            resets_at: self.resets_at.clone(),
        }
    }

    /// The window's absolute dollar figures, labeled, or `None` when neither
    /// `used_dollars` nor `limit_dollars` resolves to a number.
    fn dollars(&self, label: &str) -> Option<WindowDollars> {
        let used = self.used_dollars.as_ref().and_then(json_to_dollars);
        let limit = self.limit_dollars.as_ref().and_then(json_to_dollars);
        (used.is_some() || limit.is_some()).then(|| WindowDollars {
            label: label.to_string(),
            used,
            limit,
        })
    }
}

/// One entry of the `/usage` `limits[]` array. `kind` selects the window
/// (`session` → 5h, `weekly_all` → 7d, `weekly_scoped` → per-model); `scope`
/// carries the model name for scoped entries. `is_active` is intentionally not
/// read — the array is already scoped to what applies, and 5h/7d must show
/// regardless of it.
#[derive(Deserialize)]
struct RawLimit {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    scope: Option<RawScope>,
}

#[derive(Deserialize)]
struct RawScope {
    #[serde(default)]
    model: Option<RawScopeModel>,
    /// Consumption surface (web / desktop / code / …) for a surface-scoped
    /// limit. Undetermined shape (string vs `{id, display_name}`), so held as
    /// raw JSON and read leniently by [`RawScope::label`].
    #[serde(default)]
    surface: Option<serde_json::Value>,
}

impl RawScope {
    /// Human label for a scoped limit: the model's display name when present,
    /// else the surface name (string, or an object's `display_name`/`id`).
    fn label(&self) -> Option<String> {
        if let Some(name) = self.model.as_ref().and_then(|m| m.display_name.as_deref()) {
            return Some(name.to_string());
        }
        let surface = self.surface.as_ref()?;
        if let Some(s) = surface.as_str() {
            return Some(s.to_string());
        }
        let obj = surface.as_object()?;
        obj.get("display_name")
            .or_else(|| obj.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
}

#[derive(Deserialize)]
struct RawScopeModel {
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct RawSpend {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    used: Option<RawMoney>,
    #[serde(default)]
    limit: Option<RawMoney>,
    #[serde(default)]
    percent: Option<f64>,
}

/// A minor-unit money object (`{amount_minor, currency, exponent}`) as returned
/// under `spend`. `exponent` defaults to 2 (cents) when absent.
#[derive(Deserialize)]
struct RawMoney {
    #[serde(default)]
    amount_minor: Option<i64>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    exponent: Option<i32>,
}

impl RawMoney {
    fn to_dollars(&self) -> Option<f64> {
        Some(self.amount_minor? as f64 / 10f64.powi(self.exponent.unwrap_or(2)))
    }
}

/// The window set derived from a parsed `/usage` body.
#[derive(Default)]
struct DerivedWindows {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
    weekly_scoped: Vec<ScopedWindow>,
    window_dollars: Vec<WindowDollars>,
}

/// Derive the window set from a parsed `/usage` body. `limits[]` is the source
/// of truth: `session` → 5h, `weekly_all` → 7d, and each `weekly_scoped` entry
/// becomes a dynamic `"7d <model>"` window (from the scope's model or surface
/// name) — so a model the server adds later is picked up automatically. A
/// missing `session` / `weekly_all` limit falls back to the legacy top-level
/// field, which is also the only carrier of the per-window `*_dollars` figures.
fn windows_from_raw(raw: &RawUsage) -> DerivedWindows {
    let mut five = None;
    let mut seven = None;
    let mut scoped = Vec::new();
    for limit in &raw.limits {
        let window = UsageWindow {
            utilization: limit.percent.unwrap_or(0.0),
            resets_at: limit.resets_at.clone(),
        };
        match limit.kind.as_deref() {
            Some("session") => five = Some(window),
            Some("weekly_all") => seven = Some(window),
            Some("weekly_scoped") => {
                if let Some(name) = limit.scope.as_ref().and_then(RawScope::label) {
                    scoped.push(ScopedWindow {
                        label: format!("{LABEL_7D} {}", name.to_lowercase()),
                        window,
                    });
                }
            }
            _ => {}
        }
    }
    // Dollar figures ride only the top-level window objects, never `limits[]`.
    let window_dollars = [(LABEL_5H, &raw.five_hour), (LABEL_7D, &raw.seven_day)]
        .into_iter()
        .filter_map(|(label, w)| w.as_ref().and_then(|w| w.dollars(label)))
        .collect();
    DerivedWindows {
        five_hour: five.or_else(|| raw.five_hour.as_ref().map(RawWindow::to_window)),
        seven_day: seven.or_else(|| raw.seven_day.as_ref().map(RawWindow::to_window)),
        weekly_scoped: scoped,
        window_dollars,
    }
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
    /// present (delta-seconds or an IMF HTTP-date); an unparseable value is
    /// absent, and a `0` / past date parses to `ZERO` ("retry now").
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

/// `User-Agent` for `/usage` + `/profile` requests. Anthropic rate-limits this
/// endpoint far harder for clients that don't identify as Claude Code
/// (anthropics/claude-code#31637), so mimic its UA using the locally-detected CC
/// version (resolved once per process), falling back to a bare `claude-code`.
static USER_AGENT: LazyLock<String> =
    LazyLock::new(|| match crate::plugin_probe::cc_version().as_deref() {
        Some(v) => match v.split_whitespace().next() {
            Some(ver) if !ver.is_empty() => format!("claude-code/{ver}"),
            _ => "claude-code".to_string(),
        },
        None => "claude-code".to_string(),
    });

fn get_json(
    url: &str,
    access_token: &str,
    activity: Option<&ActivityStore>,
    name: &str,
) -> std::result::Result<String, FetchError> {
    await_request_slot(ANTHROPIC_ORIGIN);
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
        .header("User-Agent", USER_AGENT.as_str())
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

    let windows = windows_from_raw(&raw);
    let spend = raw.spend.as_ref().map(SpendInfo::from_raw);

    Ok(UsageInfo {
        plan,
        five_hour: windows.five_hour,
        seven_day: windows.seven_day,
        weekly_scoped: windows.weekly_scoped,
        window_dollars: windows.window_dollars,
        extra_usage: raw.extra_usage,
        spend,
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
