//! Token-usage statistics for the "tokens" TUI tab.
//!
//! # Design
//!
//! Two-phase load, emitted as two events per run so the tab paints instantly:
//! 1. **Base stats** ([`load_base`]) — parsed from `~/.claude/stats-cache.json`,
//!    which Claude Code maintains as a pre-aggregated lifetime snapshot. A single
//!    small JSON file, so it returns in well under a millisecond and is the tab's
//!    first paint instead of a blank "reading ~/.claude".
//! 2. **Live top-up** ([`merge_topup`]) — appends days from `~/.claude/projects/`
//!    transcripts strictly newer than the cache's `lastComputedDate`, avoiding
//!    double-counting days already in the snapshot. The recursive sweep also
//!    reaches subagent/workflow transcripts under `<session>/subagents/`. Each
//!    response is deduplicated by `message.id` (a content composite when absent,
//!    so an id-less line still dedups), and each message by line `uuid`, so a
//!    response mirrored into more than one file is counted once. Beyond tokens,
//!    the top-up reconstructs post-cutoff message/session/hour counts so the
//!    lifetime card and activity graph track the same live window as the token
//!    bars rather than freezing at `lastComputedDate`.
//!
//! The background sweep keeps a per-file contribution cache ([`TopUpCache`]) so
//! each 90s refresh re-reads only transcripts whose mtime advanced — the rest are
//! re-merged from memory, avoiding a full multi-hundred-MB re-read every cycle.
//!
//! # Caveat
//!
//! `stats-cache.json` reflects **all** Claude Code usage across all profiles and
//! accounts on this machine (global pool). The top-up reads every JSONL file
//! reachable via the home directory at load time, which may span all profiles if
//! they share a home. This is intentional — the tokens tab shows aggregate usage.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::usage::{epoch_secs_to_iso, iso_to_epoch_secs, now_epoch_secs};

// ── Refresh cadence ─────────────────────────────────────────────────────────

const REFRESH_INTERVAL: Duration = Duration::from_secs(90);

// ── Public types ─────────────────────────────────────────────────────────────

/// Per-model lifetime aggregate. `input`/`output` exclude cache; cache is separate.
#[derive(Debug, Clone, Default)]
pub(crate) struct ModelTokens {
    pub(crate) model: String,
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_create: u64,
}

impl ModelTokens {
    /// input + output (matches stats-cache `dailyModelTokens` semantics).
    pub(crate) fn in_out(&self) -> u64 {
        self.input.saturating_add(self.output)
    }

    /// input + output + cache_read + cache_create.
    pub(crate) fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_create)
    }
}

/// Per-day in+out token total summed across all models that day.
#[derive(Debug, Clone)]
pub(crate) struct DayTokens {
    pub(crate) date: String, // "YYYY-MM-DD"
    pub(crate) tokens: u64,
}

/// One model's tokens on one day. stats-cache days publish only the combined
/// in+out per model, so `split` is `None` for them; transcript-derived
/// (post-cutoff) days carry the full in/out/cache split.
#[derive(Debug, Clone)]
pub(crate) struct DayModelTokens {
    pub(crate) date: String, // "YYYY-MM-DD"
    pub(crate) model: String,
    pub(crate) in_out: u64,
    /// Full split when known; `split.model` mirrors `model`.
    pub(crate) split: Option<ModelTokens>,
}

/// One model's aggregate over a date range ([`period_models`]). `split` sums
/// only the split-bearing days, so it is a floor unless `split_complete` —
/// cache figures and cost are exact only when every day in range carried one.
#[derive(Debug, Clone)]
pub(crate) struct PeriodModel {
    pub(crate) model: String,
    pub(crate) in_out: u64,
    pub(crate) split: ModelTokens,
    pub(crate) split_complete: bool,
}

impl PeriodModel {
    /// Wrap a fully-known aggregate (lifetime / today rows) so every Tokens
    /// view ranks and renders through one row type.
    pub(crate) fn from_full(m: &ModelTokens) -> Self {
        Self {
            model: m.model.clone(),
            in_out: m.in_out(),
            split: m.clone(),
            split_complete: true,
        }
    }

    /// Display/ranking metric. Cache joins the count only when the split is
    /// fully known — callers pass `count_cache && all rows complete` so a
    /// partial split never mixes bases across rows of one list.
    pub(crate) fn metric(&self, count_cache: bool) -> u64 {
        if count_cache && self.split_complete {
            self.split.total()
        } else {
            self.in_out
        }
    }
}

/// The `count_cache` basis actually usable for a row list: cache joins the
/// counts only when every row's split is fully known, so one list never mixes
/// bases across rows.
pub(crate) fn effective_cache_basis(rows: &[PeriodModel], count_cache: bool) -> bool {
    count_cache && rows.iter().all(|m| m.split_complete)
}

/// Per-day activity counts (from stats-cache `dailyActivity`).
#[derive(Debug, Clone)]
pub(crate) struct DayActivity {
    pub(crate) date: String,
    pub(crate) messages: u64,
    pub(crate) sessions: u64,
    pub(crate) tool_calls: u64,
}

/// Single-day token + message rollup, built live from today's transcripts during
/// the top-up pass (so it carries the full in/out/cache split, unlike `DayTokens`).
#[derive(Debug, Clone, Default)]
pub(crate) struct DaySummary {
    pub(crate) date: String,
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_create: u64,
    /// User/assistant messages seen for the day (transcript-derived).
    pub(crate) messages: u64,
    /// Message count per hour of day for the day, index = hour 0..23 — the
    /// daily-period twin of `TokenStats::hour_counts`.
    pub(crate) hours: [u64; 24],
    /// Per-model breakdown of the day's tokens. Carried so cost can be priced
    /// per model (rates differ by family) — the day's lifetime totals can't be
    /// isolated from `TokenStats::models`. Empty until the top-up populates it.
    pub(crate) models: Vec<ModelTokens>,
}

impl DaySummary {
    pub(crate) fn in_out(&self) -> u64 {
        self.input.saturating_add(self.output)
    }

    pub(crate) fn total(&self) -> u64 {
        self.in_out()
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_create)
    }
}

/// Aggregated token statistics view-model.
#[derive(Debug, Clone, Default)]
pub(crate) struct TokenStats {
    /// All models individually, sorted DESC by total(). Grouping is a render concern.
    pub(crate) models: Vec<ModelTokens>,
    pub(crate) daily: Vec<DayTokens>, // chronological ASC by date
    /// Per-day per-model tokens, ASC by (date, model) — feeds the weekly /
    /// monthly period lens. Pre-cutoff entries are in+out only (see
    /// [`DayModelTokens::split`]).
    pub(crate) daily_models: Vec<DayModelTokens>,
    pub(crate) activity: Vec<DayActivity>, // chronological ASC by date
    pub(crate) hour_counts: [u64; 24],     // index = hour of day 0..23
    pub(crate) total_input: u64,
    pub(crate) total_output: u64,
    pub(crate) total_cache_read: u64,
    pub(crate) total_cache_create: u64,
    pub(crate) total_sessions: u64,
    pub(crate) total_messages: u64,
    pub(crate) first_session_date: Option<String>, // raw ISO from stats-cache
    pub(crate) last_computed_date: Option<String>, // "YYYY-MM-DD" from stats-cache
    pub(crate) topped_up_through: Option<String>,  // latest "YYYY-MM-DD" added by top-up
    /// Today's usage, built live from transcripts; `None` when idle today.
    pub(crate) today: Option<DaySummary>,
}

impl TokenStats {
    /// input + output across all models — the "work" metric that matches the
    /// daily trend (`dailyModelTokens` is in+out only). The dashboard headlines
    /// use this so today/total/daily/models all share one basis; cache is shown
    /// separately (cache-hit badge + composition card).
    pub(crate) fn total_in_out(&self) -> u64 {
        self.total_input.saturating_add(self.total_output)
    }

    /// input + output + cache_read + cache_create across all models — the full
    /// throughput, used only by the cache lens (composition card, cache-hit).
    pub(crate) fn total_tokens(&self) -> u64 {
        self.total_input
            .saturating_add(self.total_output)
            .saturating_add(self.total_cache_read)
            .saturating_add(self.total_cache_create)
    }

    /// Cache-hit ratio in 0.0..=1.0: cache_read / (cache_read + cache_create + input).
    /// Returns 0.0 when the denominator is 0.
    pub(crate) fn cache_hit_ratio(&self) -> f64 {
        let denom = self
            .total_cache_read
            .saturating_add(self.total_cache_create)
            .saturating_add(self.total_input);
        if denom == 0 {
            return 0.0;
        }
        self.total_cache_read as f64 / denom as f64
    }
}

/// Models below this lifetime total fold into the "others" row.
const OTHERS_THRESHOLD: u64 = 1_000_000;

/// Display grouping: keep Anthropic models individual, keep any other model that
/// has moved more than [`OTHERS_THRESHOLD`] tokens individual too, and fold only
/// the long tail of tiny non-Anthropic models into one "others" row.
/// Returns rows sorted DESC by `in_out()`. Pure fn, unit-testable.
pub(crate) fn group_models(models: &[ModelTokens]) -> Vec<ModelTokens> {
    let mut out: Vec<ModelTokens> = Vec::new();
    let mut others = ModelTokens {
        model: "others".to_owned(),
        ..Default::default()
    };

    for m in models {
        if is_anthropic(&m.model) || m.total() > OTHERS_THRESHOLD {
            out.push(m.clone());
        } else {
            others.input = others.input.saturating_add(m.input);
            others.output = others.output.saturating_add(m.output);
            others.cache_read = others.cache_read.saturating_add(m.cache_read);
            others.cache_create = others.cache_create.saturating_add(m.cache_create);
        }
    }

    if others.total() > 0 {
        out.push(others);
    }

    // Rank by in+out ("work"), matching the dashboard's token basis, so the
    // bars descend by the value actually shown.
    out.sort_unstable_by_key(|m| std::cmp::Reverse(m.in_out()));
    out
}

/// True when the model name denotes an Anthropic model (starts with "claude").
pub(crate) fn is_anthropic(model: &str) -> bool {
    model.starts_with("claude")
}

/// Friendly display name for a model id — the single place that maps raw ids
/// to nice labels, used everywhere a model is shown.
///
/// Anthropic ids collapse to `family version` (`claude-opus-4-8` → `opus 4.8`,
/// `claude-sonnet-4-5-20250929` → `sonnet 4.5`, `claude-opus-4-6-thinking`
/// → `opus 4.6 thinking`). A trailing 8-digit date stamp is dropped. The
/// `others` bucket and any unrecognized id pass through (date-stripped).
pub(crate) fn model_display_name(model: &str) -> String {
    if model == "others" {
        return "others".to_string();
    }
    // Drop a trailing 8-digit date stamp (e.g. `-20250929`); version segments
    // are 1–2 digits, so this never eats a real version component.
    let base = match model.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => model,
    };
    let Some(rest) = base.strip_prefix("claude-") else {
        return base.to_string();
    };
    let mut parts = rest.split('-');
    let Some(family) = parts.next() else {
        return base.to_string();
    };
    // Leading numeric/dotted segments form the version (joined by `.`); any
    // trailing words (e.g. `thinking`) are appended verbatim.
    let mut version: Vec<&str> = Vec::new();
    let mut extras: Vec<&str> = Vec::new();
    for p in parts {
        if extras.is_empty() && !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        {
            version.push(p);
        } else {
            extras.push(p);
        }
    }
    let mut out = family.to_string();
    if !version.is_empty() {
        out.push(' ');
        out.push_str(&version.join("."));
    }
    for e in extras {
        out.push(' ');
        out.push_str(e);
    }
    out
}

// ── stats-cache.json wire types ──────────────────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct StatsCacheFile {
    last_computed_date: Option<String>,
    first_session_date: Option<String>,
    total_sessions: u64,
    total_messages: u64,
    daily_activity: Vec<WireActivity>,
    daily_model_tokens: Vec<WireDayTokens>,
    model_usage: HashMap<String, WireModelUsage>,
    hour_counts: HashMap<String, u64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct WireActivity {
    date: String,
    message_count: u64,
    session_count: u64,
    tool_call_count: u64,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct WireDayTokens {
    date: String,
    tokens_by_model: HashMap<String, u64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct WireModelUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
}

// ── JSONL transcript wire types ──────────────────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(default)]
struct TranscriptLine {
    timestamp: Option<String>,
    uuid: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    message: Option<TranscriptMsg>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TranscriptMsg {
    id: Option<String>,
    model: Option<String>,
    role: Option<String>,
    usage: Option<TranscriptUsage>,
    content: Option<Vec<ContentBlock>>,
}

/// One content block of an assistant message; only its `type` is read, to count
/// `tool_use` invocations for the per-day tool-call total.
#[derive(Deserialize, Default)]
#[serde(default)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TranscriptUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
}

// ── Load ─────────────────────────────────────────────────────────────────────

/// Parse `stats-cache.json` into the base view-model — the fast first phase of
/// the load. No transcript sweep, so it returns in well under a millisecond; the
/// background loader emits this for an instant first paint, then merges the live
/// transcript top-up ([`merge_topup`]). Returns `None` when the cache file is
/// missing or unparseable.
pub(crate) fn load_base(claude_dir: &Path) -> Option<TokenStats> {
    let cache_path = claude_dir.join("stats-cache.json");
    let raw = std::fs::read_to_string(&cache_path).ok()?;
    let wire: StatsCacheFile = serde_json::from_str(&raw).ok()?;

    let mut models: Vec<ModelTokens> = wire
        .model_usage
        .iter()
        .map(|(name, u)| ModelTokens {
            model: name.clone(),
            input: u.input_tokens,
            output: u.output_tokens,
            cache_read: u.cache_read_input_tokens,
            cache_create: u.cache_creation_input_tokens,
        })
        .collect();
    models.sort_unstable_by_key(|m| std::cmp::Reverse(m.total()));

    let mut daily: Vec<DayTokens> = wire
        .daily_model_tokens
        .iter()
        .map(|d| DayTokens {
            date: d.date.clone(),
            tokens: d.tokens_by_model.values().copied().sum(),
        })
        .collect();
    daily.sort_unstable_by_key(|d| d.date.clone());

    let mut daily_models: Vec<DayModelTokens> = wire
        .daily_model_tokens
        .iter()
        .flat_map(|d| {
            d.tokens_by_model
                .iter()
                .map(|(model, &in_out)| DayModelTokens {
                    date: d.date.clone(),
                    model: model.clone(),
                    in_out,
                    split: None,
                })
        })
        .collect();
    daily_models.sort_unstable_by(|a, b| {
        (a.date.as_str(), a.model.as_str()).cmp(&(b.date.as_str(), b.model.as_str()))
    });

    let mut activity: Vec<DayActivity> = wire
        .daily_activity
        .iter()
        .map(|a| DayActivity {
            date: a.date.clone(),
            messages: a.message_count,
            sessions: a.session_count,
            tool_calls: a.tool_call_count,
        })
        .collect();
    activity.sort_unstable_by_key(|d| d.date.clone());

    let mut hour_counts = [0u64; 24];
    for (k, v) in &wire.hour_counts {
        if let Ok(h) = k.parse::<usize>()
            && h < 24
        {
            hour_counts[h] = *v;
        }
    }

    let total_input: u64 = wire.model_usage.values().map(|u| u.input_tokens).sum();
    let total_output: u64 = wire.model_usage.values().map(|u| u.output_tokens).sum();
    let total_cache_read: u64 = wire
        .model_usage
        .values()
        .map(|u| u.cache_read_input_tokens)
        .sum();
    let total_cache_create: u64 = wire
        .model_usage
        .values()
        .map(|u| u.cache_creation_input_tokens)
        .sum();

    let stats = TokenStats {
        models,
        daily,
        daily_models,
        activity,
        hour_counts,
        total_input,
        total_output,
        total_cache_read,
        total_cache_create,
        total_sessions: wire.total_sessions,
        total_messages: wire.total_messages,
        first_session_date: wire.first_session_date,
        last_computed_date: wire.last_computed_date.clone(),
        topped_up_through: None,
        today: None,
    };

    Some(stats)
}

/// Full synchronous load: parse the stats-cache, sweep transcripts once, merge.
/// Test-only — the background [`spawn`] loader uses `load_base` + `merge_topup`
/// directly so it can paint the base instantly and reuse its per-file cache.
#[cfg(test)]
pub(crate) fn load(claude_dir: &Path) -> Option<TokenStats> {
    let mut base = load_base(claude_dir)?;
    let today = today_date();
    let lcd = base.last_computed_date.clone();
    let mut cache = TopUpCache::default();
    refresh_topup_cache(claude_dir, lcd.as_deref(), &mut cache);
    merge_topup(&mut base, &cache, lcd.as_deref(), &today);
    Some(base)
}

/// Current UTC calendar date as "YYYY-MM-DD".
pub(crate) fn today_date() -> String {
    let iso = epoch_secs_to_iso(now_epoch_secs());
    iso.get(..10).map(str::to_owned).unwrap_or(iso)
}

// ── Period bucketing ─────────────────────────────────────────────────────────

/// Calendar bucket granularity for the weekly / monthly period lens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bucket {
    Week,
    Month,
}

/// First day of `date`'s bucket — monday for weeks, the 1st for months.
/// Degrades to `date` itself on an unparseable input.
pub(crate) fn bucket_start(date: &str, bucket: Bucket) -> String {
    match bucket {
        Bucket::Month if date.len() >= 7 => format!("{}-01", &date[..7]),
        Bucket::Month => date.to_owned(),
        Bucket::Week => {
            let Some(secs) = iso_to_epoch_secs(&format!("{date}T00:00:00+00:00")) else {
                return date.to_owned();
            };
            let days = secs.div_euclid(86_400);
            // 1970-01-01 was a thursday; monday-indexed weekday.
            let weekday = (days + 3).rem_euclid(7);
            let iso = epoch_secs_to_iso((days - weekday) * 86_400);
            iso.get(..10).map(str::to_owned).unwrap_or(iso)
        }
    }
}

/// Inclusive (from, to) date range of the current bucket containing `today`.
pub(crate) fn current_bucket_bounds(today: &str, bucket: Bucket) -> (String, String) {
    (bucket_start(today, bucket), today.to_owned())
}

/// Fold chronological-ASC daily totals into bucket totals, one row per bucket
/// keyed (and dated) by the bucket's first day. Adjacent-fold, so the input
/// order invariant of [`TokenStats::daily`] is load-bearing.
pub(crate) fn bucket_tokens(days: &[DayTokens], bucket: Bucket) -> Vec<DayTokens> {
    let mut out: Vec<DayTokens> = Vec::new();
    for d in days {
        let key = bucket_start(&d.date, bucket);
        match out.last_mut() {
            Some(last) if last.date == key => {
                last.tokens = last.tokens.saturating_add(d.tokens);
            }
            _ => out.push(DayTokens {
                date: key,
                tokens: d.tokens,
            }),
        }
    }
    out
}

/// [`bucket_tokens`]'s activity twin. Bucket sessions are sums of daily counts,
/// so a session spanning days counts once per day it touched — a known ceiling,
/// matching how the per-day rows already report it.
pub(crate) fn bucket_activity(days: &[DayActivity], bucket: Bucket) -> Vec<DayActivity> {
    let mut out: Vec<DayActivity> = Vec::new();
    for d in days {
        let key = bucket_start(&d.date, bucket);
        match out.last_mut() {
            Some(last) if last.date == key => {
                last.messages = last.messages.saturating_add(d.messages);
                last.sessions = last.sessions.saturating_add(d.sessions);
                last.tool_calls = last.tool_calls.saturating_add(d.tool_calls);
            }
            _ => out.push(DayActivity {
                date: key,
                ..d.clone()
            }),
        }
    }
    out
}

/// Aggregate per-day per-model rows over the inclusive `from..=to` date range,
/// ranked DESC by in+out. See [`PeriodModel`] for the split-floor semantics.
pub(crate) fn period_models(days: &[DayModelTokens], from: &str, to: &str) -> Vec<PeriodModel> {
    let mut map: HashMap<&str, PeriodModel> = HashMap::new();
    for d in days {
        if d.date.as_str() < from || d.date.as_str() > to {
            continue;
        }
        let e = map.entry(d.model.as_str()).or_insert_with(|| PeriodModel {
            model: d.model.clone(),
            in_out: 0,
            split: ModelTokens {
                model: d.model.clone(),
                ..Default::default()
            },
            split_complete: true,
        });
        e.in_out = e.in_out.saturating_add(d.in_out);
        match &d.split {
            Some(s) => {
                e.split.input = e.split.input.saturating_add(s.input);
                e.split.output = e.split.output.saturating_add(s.output);
                e.split.cache_read = e.split.cache_read.saturating_add(s.cache_read);
                e.split.cache_create = e.split.cache_create.saturating_add(s.cache_create);
            }
            None => e.split_complete = false,
        }
    }
    let mut out: Vec<PeriodModel> = map.into_values().collect();
    out.sort_unstable_by_key(|m| std::cmp::Reverse(m.in_out));
    out
}

// ── Recent transcript top-up ─────────────────────────────────────────────────

/// Derive a `SystemTime` cutoff from a "YYYY-MM-DD" string (00:00 UTC of that day).
fn date_to_cutoff(date: &str) -> Option<SystemTime> {
    let ts = format!("{date}T00:00:00+00:00");
    let secs = iso_to_epoch_secs(&ts)?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Recursively collect `*.jsonl` paths under `dir`, descending at most
/// `max_depth` directory levels. Subagent and workflow transcripts are nested
/// under `projects/<slug>/<session>/subagents/`, deeper than the main-session
/// files, so a single-level read would miss them. `DirEntry::file_type` does not
/// follow symlinks, so a symlinked directory is treated as a file and never
/// recursed — this bounds the walk and avoids cycles.
fn collect_jsonl(dir: &Path, max_depth: usize, out: &mut Vec<PathBuf>) {
    if max_depth == 0 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            collect_jsonl(&path, max_depth - 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// One transcript line's contribution, cached per file so an unchanged file is
/// re-merged from memory instead of re-read on the next 90s sweep. The dedup keys
/// are carried so the cross-file merge counts each response / message once even
/// when resumed or forked sessions copy lines forward.
struct LineRec {
    date: String,    // "YYYY-MM-DD"
    hour: u8,        // 0..=23
    uuid: String,    // line uuid ("" when absent) — message/hour/session dedup
    session: String, // sessionId ("" when absent)
    /// A user/assistant turn — counts toward messages, hours, sessions.
    is_message: bool,
    /// Token fields below are valid only when set (an assistant `usage` line).
    has_usage: bool,
    tok_key: String, // message.id, or a composite when absent — token dedup
    model: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
    tool_calls: u64,
}

struct FileContrib {
    mtime: SystemTime,
    recs: Vec<LineRec>,
}

/// Per-file transcript contributions persisted across the loader's 90s sweeps.
/// Only files whose mtime advanced (or are new) get re-read; everything else is
/// re-merged from memory, so the multi-hundred-MB full re-read that the old sweep
/// paid every cycle is gone. Bounded by the post-cutoff transcript volume of the
/// session — a few MB; rebuilt fresh on a cold start.
#[derive(Default)]
struct TopUpCache {
    files: HashMap<PathBuf, FileContrib>,
}

/// Re-stat every `*.jsonl` under `projects/` (recursive, so subagent and workflow
/// transcripts count too) and re-read only those modified since the last sweep,
/// refreshing their cached records. Files at/under the cutoff or no longer present
/// are dropped. The stat pass is cheap; the file IO that dominated the old sweep
/// now runs only for what actually changed between ticks.
fn refresh_topup_cache(
    claude_dir: &Path,
    last_computed_date: Option<&str>,
    cache: &mut TopUpCache,
) {
    let Some(cutoff_date) = last_computed_date else {
        cache.files.clear();
        return;
    };
    let Some(cutoff_st) = date_to_cutoff(cutoff_date) else {
        return;
    };
    let projects_dir = claude_dir.join("projects");
    let mut paths: Vec<PathBuf> = Vec::new();
    collect_jsonl(&projects_dir, 8, &mut paths);

    // Drop cache entries for files that vanished since the last sweep.
    let present: HashSet<&PathBuf> = paths.iter().collect();
    cache.files.retain(|p, _| present.contains(p));

    for path in paths {
        let mtime = match std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
        {
            Some(t) => t,
            None => {
                cache.files.remove(&path);
                continue;
            }
        };
        // mtime guard: a file untouched since the cutoff can hold no post-cutoff
        // lines, so it never enters the cache (and its today lines, if any, would
        // also predate the cutoff day).
        if mtime <= cutoff_st {
            cache.files.remove(&path);
            continue;
        }
        match cache.files.get(&path) {
            Some(fc) if fc.mtime == mtime => {} // unchanged — keep cached records
            _ => {
                let recs = parse_file(&path);
                cache.files.insert(path, FileContrib { mtime, recs });
            }
        }
    }
}

/// Merge the cached per-file records into `base` (already holding stats-cache
/// data). The cutoff/today split is applied here, not at parse time, so an
/// advancing `last_computed_date` needs no re-read — the same records just flow to
/// different buckets. Responses are deduped by token key, messages by line uuid,
/// across all files. Token totals/models/daily extend the base; messages,
/// sessions, hour buckets, and per-day activity for days strictly after the cutoff
/// are reconstructed from transcripts (close to, but not byte-identical with,
/// Claude Code's own counters) so the lifetime card and activity graph track the
/// same live window as the token bars instead of freezing at the cutoff.
fn merge_topup(
    base: &mut TokenStats,
    cache: &TopUpCache,
    last_computed_date: Option<&str>,
    today_date: &str,
) {
    let Some(cutoff_date) = last_computed_date else {
        return; // no safe boundary
    };

    // Token aggregates seeded from base so post-cutoff days extend, never replace.
    let mut daily_map: HashMap<String, u64> = base
        .daily
        .iter()
        .map(|d| (d.date.clone(), d.tokens))
        .collect();
    let mut model_map: HashMap<String, ModelTokens> = base
        .models
        .iter()
        .cloned()
        .map(|m| (m.model.clone(), m))
        .collect();

    // Per-day per-model splits for post-cutoff days (weekly/monthly lens).
    let mut day_models: HashMap<(String, String), ModelTokens> = HashMap::new();
    // Per-day activity for post-cutoff days + lifetime deltas.
    let mut day_msgs: HashMap<String, u64> = HashMap::new();
    let mut day_sessions: HashMap<String, HashSet<String>> = HashMap::new();
    let mut day_tools: HashMap<String, u64> = HashMap::new();
    let mut post_sessions: HashSet<String> = HashSet::new();
    let mut new_hours = [0u64; 24];

    let mut today_acc = DaySummary {
        date: today_date.to_owned(),
        ..Default::default()
    };
    let mut today_models: HashMap<String, ModelTokens> = HashMap::new();

    let mut seen_tok: HashSet<&str> = HashSet::new();
    let mut seen_uuid: HashSet<&str> = HashSet::new();
    let mut max_date: Option<String> = None;

    for fc in cache.files.values() {
        for r in &fc.recs {
            // Message / hour / session counting (user+assistant), deduped by uuid.
            // An empty uuid can't be keyed, so it counts as-is.
            if r.is_message && (r.uuid.is_empty() || seen_uuid.insert(r.uuid.as_str())) {
                if r.date == today_date {
                    today_acc.messages = today_acc.messages.saturating_add(1);
                    if (r.hour as usize) < 24 {
                        today_acc.hours[r.hour as usize] =
                            today_acc.hours[r.hour as usize].saturating_add(1);
                    }
                }
                if r.date.as_str() > cutoff_date {
                    *day_msgs.entry(r.date.clone()).or_insert(0) += 1;
                    *day_tools.entry(r.date.clone()).or_insert(0) += r.tool_calls;
                    if (r.hour as usize) < 24 {
                        new_hours[r.hour as usize] += 1;
                    }
                    if !r.session.is_empty() {
                        day_sessions
                            .entry(r.date.clone())
                            .or_default()
                            .insert(r.session.clone());
                        post_sessions.insert(r.session.clone());
                    }
                }
            }

            // Token / model accumulation (assistant usage lines), deduped by key.
            if r.has_usage && (r.tok_key.is_empty() || seen_tok.insert(r.tok_key.as_str())) {
                if r.date == today_date {
                    today_acc.input = today_acc.input.saturating_add(r.input);
                    today_acc.output = today_acc.output.saturating_add(r.output);
                    today_acc.cache_read = today_acc.cache_read.saturating_add(r.cache_read);
                    today_acc.cache_create = today_acc.cache_create.saturating_add(r.cache_create);
                    let tm = today_models
                        .entry(r.model.clone())
                        .or_insert_with(|| ModelTokens {
                            model: r.model.clone(),
                            ..Default::default()
                        });
                    tm.input = tm.input.saturating_add(r.input);
                    tm.output = tm.output.saturating_add(r.output);
                    tm.cache_read = tm.cache_read.saturating_add(r.cache_read);
                    tm.cache_create = tm.cache_create.saturating_add(r.cache_create);
                }
                if r.date.as_str() > cutoff_date {
                    *daily_map.entry(r.date.clone()).or_insert(0) +=
                        r.input.saturating_add(r.output);
                    let dm = day_models
                        .entry((r.date.clone(), r.model.clone()))
                        .or_insert_with(|| ModelTokens {
                            model: r.model.clone(),
                            ..Default::default()
                        });
                    dm.input = dm.input.saturating_add(r.input);
                    dm.output = dm.output.saturating_add(r.output);
                    dm.cache_read = dm.cache_read.saturating_add(r.cache_read);
                    dm.cache_create = dm.cache_create.saturating_add(r.cache_create);
                    let e = model_map
                        .entry(r.model.clone())
                        .or_insert_with(|| ModelTokens {
                            model: r.model.clone(),
                            ..Default::default()
                        });
                    e.input = e.input.saturating_add(r.input);
                    e.output = e.output.saturating_add(r.output);
                    e.cache_read = e.cache_read.saturating_add(r.cache_read);
                    e.cache_create = e.cache_create.saturating_add(r.cache_create);
                    if max_date
                        .as_deref()
                        .is_none_or(|prev| r.date.as_str() > prev)
                    {
                        max_date = Some(r.date.clone());
                    }
                }
            }
        }
    }

    // Publish today's rollup (independent of the cutoff, so even a no-new-history
    // pass can still carry today's data).
    if today_acc.messages > 0 || today_acc.total() > 0 {
        today_acc.models = today_models.into_values().collect();
        today_acc
            .models
            .sort_unstable_by_key(|m| std::cmp::Reverse(m.total()));
        base.today = Some(today_acc);
    }

    if max_date.is_none() && day_msgs.is_empty() {
        return;
    }

    // Flush token daily/model back, recompute totals from the merged models.
    for (date, tokens) in &daily_map {
        if let Some(existing) = base.daily.iter_mut().find(|d| &d.date == date) {
            existing.tokens = *tokens;
        } else {
            base.daily.push(DayTokens {
                date: date.clone(),
                tokens: *tokens,
            });
        }
    }
    base.daily.sort_unstable_by_key(|d| d.date.clone());

    // Post-cutoff per-day per-model rows are transcript-authoritative: drop any
    // base rows past the cutoff (normally none — stats-cache stops there) and
    // append the reconstructed split-bearing ones.
    base.daily_models.retain(|d| d.date.as_str() <= cutoff_date);
    for ((date, _), tokens) in day_models {
        base.daily_models.push(DayModelTokens {
            date,
            model: tokens.model.clone(),
            in_out: tokens.in_out(),
            split: Some(tokens),
        });
    }
    base.daily_models.sort_unstable_by(|a, b| {
        (a.date.as_str(), a.model.as_str()).cmp(&(b.date.as_str(), b.model.as_str()))
    });

    base.models = model_map.into_values().collect();
    base.models
        .sort_unstable_by_key(|m| std::cmp::Reverse(m.total()));
    base.total_input = base.models.iter().map(|m| m.input).sum();
    base.total_output = base.models.iter().map(|m| m.output).sum();
    base.total_cache_read = base.models.iter().map(|m| m.cache_read).sum();
    base.total_cache_create = base.models.iter().map(|m| m.cache_create).sum();

    // Append post-cutoff activity days so the activity graph extends past the
    // cutoff. Base days (≤ cutoff) are authoritative and untouched.
    let mut added_msgs = 0u64;
    for (date, msgs) in &day_msgs {
        let sessions = day_sessions.get(date).map(|s| s.len() as u64).unwrap_or(0);
        let tools = day_tools.get(date).copied().unwrap_or(0);
        added_msgs = added_msgs.saturating_add(*msgs);
        if let Some(a) = base.activity.iter_mut().find(|a| &a.date == date) {
            a.messages = *msgs;
            a.sessions = sessions;
            a.tool_calls = tools;
        } else {
            base.activity.push(DayActivity {
                date: date.clone(),
                messages: *msgs,
                sessions,
                tool_calls: tools,
            });
        }
    }
    base.activity.sort_unstable_by_key(|d| d.date.clone());

    // Lifetime deltas. Sessions span days, so the total uses the global distinct
    // post-cutoff set (not the per-day sum). A session straddling the cutoff is
    // counted in both base and here — rare, since sessions are short-lived.
    base.total_messages = base.total_messages.saturating_add(added_msgs);
    base.total_sessions = base
        .total_sessions
        .saturating_add(post_sessions.len() as u64);
    for (h, c) in new_hours.iter().enumerate() {
        base.hour_counts[h] = base.hour_counts[h].saturating_add(*c);
    }

    base.topped_up_through = max_date;
}

/// Parse one JSONL transcript into per-line contribution records. Pure read —
/// the cutoff/today split happens later in [`merge_topup`], so an advancing
/// `last_computed_date` never forces a re-read. Silently skips parse errors.
fn parse_file(path: &Path) -> Vec<LineRec> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = std::io::BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let parsed: TranscriptLine = match serde_json::from_str(&line) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let Some(ts) = parsed.timestamp.as_deref() else {
            continue;
        };
        if ts.len() < 10 {
            continue;
        }
        let date = ts[..10].to_owned();
        // ISO 8601 `YYYY-MM-DDThh:mm:ss…` — hour is bytes 11..13.
        let hour = ts
            .get(11..13)
            .and_then(|h| h.parse::<u8>().ok())
            .filter(|h| *h < 24)
            .unwrap_or(0);
        let Some(msg) = parsed.message.as_ref() else {
            continue;
        };
        let role = msg.role.as_deref().unwrap_or("");
        let usage = msg.usage.as_ref();
        // A user/assistant turn counts as a message; a usage-bearing line with no
        // role (some synthesized/streaming entries) is still a real response, so
        // count it too — in a normal transcript every usage line is role=assistant.
        let is_message = role == "user" || role == "assistant" || usage.is_some();
        if !is_message {
            continue;
        }

        let (has_usage, tok_key, model, input, output, cache_read, cache_create) =
            if let Some(u) = usage {
                let model = msg.model.clone().unwrap_or_else(|| "unknown".to_owned());
                // Dedup by `message.id` (present on every usage line); fall back to
                // a content-derived composite when absent so an id-less line still
                // dedups instead of bypassing the guard (the old over-count bug,
                // hit by usage lines that carry no `requestId`).
                let key = match msg.id.as_deref() {
                    Some(id) if !id.is_empty() => id.to_owned(),
                    _ => format!(
                        "{date}|{model}|{}|{}|{}|{}",
                        u.input_tokens,
                        u.output_tokens,
                        u.cache_read_input_tokens,
                        u.cache_creation_input_tokens
                    ),
                };
                (
                    true,
                    key,
                    model,
                    u.input_tokens,
                    u.output_tokens,
                    u.cache_read_input_tokens,
                    u.cache_creation_input_tokens,
                )
            } else {
                (false, String::new(), String::new(), 0, 0, 0, 0)
            };

        let tool_calls = msg
            .content
            .as_ref()
            .map(|c| {
                c.iter()
                    .filter(|b| b.kind.as_deref() == Some("tool_use"))
                    .count() as u64
            })
            .unwrap_or(0);

        out.push(LineRec {
            date,
            hour,
            uuid: parsed.uuid.clone().unwrap_or_default(),
            session: parsed.session_id.clone().unwrap_or_default(),
            is_message,
            has_usage,
            tok_key,
            model,
            input,
            output,
            cache_read,
            cache_create,
            tool_calls,
        });
    }
    out
}

// ── Background thread ─────────────────────────────────────────────────────────

/// Events emitted by the background loader thread.
pub(crate) enum TokensEvent {
    /// Phase 1: stats-cache parsed, transcript sweep not yet run. Lets the tab
    /// paint lifetime/model data instantly instead of blocking on the sweep.
    Base(Box<TokenStats>),
    /// Phase 2: the live transcript top-up merged in (today card, recent days,
    /// reconstructed lifetime counts). Supersedes the matching `Base`.
    Loaded(Box<TokenStats>),
    Failed,
}

/// Spawn the token-stats background worker. Each run emits two events: `Base`
/// (the instant stats-cache parse) then `Loaded` (after the transcript top-up).
/// Loads once on start, then loops on `refresh_rx.recv_timeout(REFRESH_INTERVAL)`,
/// reusing a per-file cache so each sweep re-reads only changed transcripts. Exits
/// when `refresh_rx` disconnects (TUI shutdown).
///
/// Unlike `status`/`pricing`, this loop is match-first-then-send (not
/// `run_polling_loop`'s tick-first shape): the first reload after the cold load
/// happens only once the interval elapses or a signal arrives, avoiding a
/// duplicate emit at startup.
///
/// `claude_dir` must already be resolved by the caller — the worker never
/// re-resolves `home_dir()`, matching the pattern in `status::spawn`.
pub(crate) fn spawn(tx: Sender<TokensEvent>, refresh_rx: Receiver<()>, claude_dir: PathBuf) {
    std::thread::spawn(move || {
        let mut cache = TopUpCache::default();

        let run = |cache: &mut TopUpCache| {
            let Some(mut base) = load_base(&claude_dir) else {
                let _ = tx.send(TokensEvent::Failed);
                return;
            };
            // Phase 1 — instant lifetime/model data, no transcript IO.
            let _ = tx.send(TokensEvent::Base(Box::new(base.clone())));
            // Phase 2 — refresh the per-file cache (changed files only) and merge.
            let today = today_date();
            let lcd = base.last_computed_date.clone();
            refresh_topup_cache(&claude_dir, lcd.as_deref(), cache);
            merge_topup(&mut base, cache, lcd.as_deref(), &today);
            let _ = tx.send(TokensEvent::Loaded(Box::new(base)));
        };

        run(&mut cache);

        loop {
            match refresh_rx.recv_timeout(REFRESH_INTERVAL) {
                Ok(()) => while refresh_rx.try_recv().is_ok() {},
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            run(&mut cache);
        }
    });
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "../tests/inline/tokens.rs"]
mod tests;
