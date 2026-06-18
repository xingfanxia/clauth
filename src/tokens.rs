//! Token-usage statistics for the "tokens" TUI tab.
//!
//! # Design
//!
//! Two-stage load:
//! 1. **Base stats** — parsed from `~/.claude/stats-cache.json`, which Claude Code
//!    maintains as a pre-aggregated lifetime snapshot. This is fast and always
//!    available when the user has run Claude Code at least once.
//! 2. **Live top-up** — appends recent days from `~/.claude/projects/` transcripts
//!    whose mtime is strictly newer than the cache's `lastComputedDate`, avoiding
//!    double-counting days already in the snapshot. The sweep is recursive, so it
//!    also reaches subagent and workflow transcripts nested under
//!    `<session>/subagents/`, and deduplicates each assistant message by
//!    `(requestId, message.id)` so a response mirrored into more than one file
//!    (resumed/forked sessions, sidechain turns) is counted once.
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
    /// Usage-bearing (assistant) messages seen for the day.
    pub(crate) messages: u64,
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
#[derive(Debug, Clone)]
pub(crate) struct TokenStats {
    /// All models individually, sorted DESC by total(). Grouping is a render concern.
    pub(crate) models: Vec<ModelTokens>,
    pub(crate) daily: Vec<DayTokens>, // chronological ASC by date
    pub(crate) activity: Vec<DayActivity>, // chronological ASC by date
    pub(crate) hour_counts: [u64; 24], // index = hour of day 0..23
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
    // Drop a trailing 8-digit date stamp (e.g. `…-20250929`); version segments
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
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<TranscriptMsg>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TranscriptMsg {
    id: Option<String>,
    model: Option<String>,
    usage: Option<TranscriptUsage>,
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

/// Load and aggregate from a `~/.claude` directory.
/// Returns `None` if `stats-cache.json` is missing or unparseable.
/// Runs the recent transcript top-up internally.
pub(crate) fn load(claude_dir: &Path) -> Option<TokenStats> {
    let cache_path = claude_dir.join("stats-cache.json");
    let raw = std::fs::read_to_string(&cache_path).ok()?;
    let wire: StatsCacheFile = serde_json::from_str(&raw).ok()?;

    // Build models from modelUsage.
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

    // Build daily from dailyModelTokens.
    let mut daily: Vec<DayTokens> = wire
        .daily_model_tokens
        .iter()
        .map(|d| DayTokens {
            date: d.date.clone(),
            tokens: d.tokens_by_model.values().copied().sum(),
        })
        .collect();
    daily.sort_unstable_by_key(|d| d.date.clone());

    // Build activity from dailyActivity.
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

    // Build hour_counts; missing keys → 0.
    let mut hour_counts = [0u64; 24];
    for (k, v) in &wire.hour_counts {
        if let Ok(h) = k.parse::<usize>()
            && h < 24
        {
            hour_counts[h] = *v;
        }
    }

    // Compute totals from modelUsage.
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

    let mut stats = TokenStats {
        models,
        daily,
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

    let today = today_date();
    top_up(
        claude_dir,
        wire.last_computed_date.as_deref(),
        &today,
        &mut stats,
    );

    Some(stats)
}

/// Current UTC calendar date as "YYYY-MM-DD".
fn today_date() -> String {
    let iso = epoch_secs_to_iso(now_epoch_secs());
    iso.get(..10).map(str::to_owned).unwrap_or(iso)
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

/// Best-effort live top-up from every `*.jsonl` under `projects/` (recursive, so
/// subagent and workflow transcripts count too) modified after the cutoff,
/// deduplicated per assistant message by `(requestId, message.id)`. Also
/// accumulates `today_date`'s usage into `stats.today` (independent of the
/// historical cutoff, so it works even when the cache was computed today).
fn top_up(
    claude_dir: &Path,
    last_computed_date: Option<&str>,
    today_date: &str,
    stats: &mut TokenStats,
) {
    // Without a cutoff date we have no safe boundary — skip entirely.
    let cutoff_date = match last_computed_date {
        Some(d) => d,
        None => return,
    };
    let cutoff_st = match date_to_cutoff(cutoff_date) {
        Some(st) => st,
        None => return,
    };

    let projects_dir = claude_dir.join("projects");

    // daily lookup by date for fast merge.
    let mut daily_map: HashMap<String, u64> = stats
        .daily
        .iter()
        .map(|d| (d.date.clone(), d.tokens))
        .collect();

    // model lookup by name.
    let mut model_map: HashMap<String, ModelTokens> = stats
        .models
        .iter()
        .cloned()
        .map(|m| (m.model.clone(), m))
        .collect();

    let mut max_date: Option<String> = None;
    let mut today_acc = DaySummary {
        date: today_date.to_owned(),
        ..Default::default()
    };
    // Per-model split of today's usage, accumulated alongside `today_acc` so the
    // cost lens can price today per model (rates differ by family).
    let mut today_models: HashMap<String, ModelTokens> = HashMap::new();

    // Usage lines already counted this pass, keyed by `(requestId, message.id)`.
    // The same assistant response can appear in several transcripts (resumed or
    // forked sessions copy lines forward; a sidechain turn is mirrored into its
    // own subagent file), so this guards against counting one response twice.
    let mut seen: HashSet<String> = HashSet::new();

    // Recursive sweep: main-session transcripts sit at `projects/<slug>/<id>.jsonl`,
    // but subagent and workflow transcripts live deeper under
    // `projects/<slug>/<session>/subagents/`, so a flat read would miss them.
    let mut jsonl_paths: Vec<PathBuf> = Vec::new();
    collect_jsonl(&projects_dir, 8, &mut jsonl_paths);

    for path in &jsonl_paths {
        // mtime guard: skip files not modified after the cutoff.
        let mtime = match std::fs::metadata(path).ok().and_then(|m| m.modified().ok()) {
            Some(t) => t,
            None => continue,
        };
        if mtime <= cutoff_st {
            continue;
        }
        process_jsonl(
            path,
            cutoff_date,
            today_date,
            &mut daily_map,
            &mut model_map,
            &mut today_acc,
            &mut today_models,
            &mut max_date,
            &mut seen,
        );
    }

    // Publish today's rollup before any early return (it does not depend on the
    // historical cutoff, so even a no-new-history pass can still have today data).
    if today_acc.messages > 0 || today_acc.total() > 0 {
        today_acc.models = today_models.into_values().collect();
        today_acc
            .models
            .sort_unstable_by_key(|m| std::cmp::Reverse(m.total()));
        stats.today = Some(today_acc);
    }

    if max_date.is_none() {
        return;
    }

    // Flush daily_map back, preserving existing entries and appending new ones.
    for (date, tokens) in &daily_map {
        if let Some(existing) = stats.daily.iter_mut().find(|d| &d.date == date) {
            existing.tokens = *tokens;
        } else {
            stats.daily.push(DayTokens {
                date: date.clone(),
                tokens: *tokens,
            });
        }
    }
    stats.daily.sort_unstable_by_key(|d| d.date.clone());

    // Flush model_map back, recompute totals from scratch.
    stats.models = model_map.into_values().collect();
    stats
        .models
        .sort_unstable_by_key(|m| std::cmp::Reverse(m.total()));

    stats.total_input = stats.models.iter().map(|m| m.input).sum();
    stats.total_output = stats.models.iter().map(|m| m.output).sum();
    stats.total_cache_read = stats.models.iter().map(|m| m.cache_read).sum();
    stats.total_cache_create = stats.models.iter().map(|m| m.cache_create).sum();

    // Token figures (models, daily, totals) are topped up above. `total_sessions`,
    // `total_messages`, and `hour_counts` stay at stats-cache values — they are
    // lifetime aggregates that lag at most a few days and are not reconstructed
    // from transcripts (the JSONL has no comparable session/hour rollup).
    stats.topped_up_through = max_date;
}

/// Process one JSONL file: historical days strictly after `cutoff_date` flow into
/// `daily_map`/`model_map`, today's lines into `today`/`today_models`. `seen`
/// deduplicates usage lines by `(requestId, message.id)` across the whole pass.
/// Silently skips any parse errors.
#[allow(clippy::too_many_arguments)]
fn process_jsonl(
    path: &Path,
    cutoff_date: &str,
    today_date: &str,
    daily_map: &mut HashMap<String, u64>,
    model_map: &mut HashMap<String, ModelTokens>,
    today: &mut DaySummary,
    today_models: &mut HashMap<String, ModelTokens>,
    max_date: &mut Option<String>,
    seen: &mut HashSet<String>,
) {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let parsed: TranscriptLine = match serde_json::from_str(&line) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let timestamp = match &parsed.timestamp {
            Some(t) => t,
            None => continue,
        };
        if timestamp.len() < 10 {
            continue;
        }
        let date = &timestamp[..10];
        let msg = match &parsed.message {
            Some(m) => m,
            None => continue,
        };
        let usage = match &msg.usage {
            Some(u) => u,
            None => continue,
        };

        // Dedupe by `(requestId, message.id)`: the same assistant response can be
        // written to multiple transcripts, so count it once. Lines missing either
        // id can't be keyed and are left to count as-is.
        if let (Some(req), Some(id)) = (parsed.request_id.as_deref(), msg.id.as_deref()) {
            let mut dedup_key = String::with_capacity(req.len() + id.len() + 1);
            dedup_key.push_str(req);
            dedup_key.push('\0');
            dedup_key.push_str(id);
            if !seen.insert(dedup_key) {
                continue;
            }
        }

        let model_name = msg.model.as_deref().unwrap_or("unknown").to_owned();

        // Today's rollup — independent of the historical cutoff, so it is also
        // populated on the rare day the cache was last computed today.
        if date == today_date {
            today.input = today.input.saturating_add(usage.input_tokens);
            today.output = today.output.saturating_add(usage.output_tokens);
            today.cache_read = today
                .cache_read
                .saturating_add(usage.cache_read_input_tokens);
            today.cache_create = today
                .cache_create
                .saturating_add(usage.cache_creation_input_tokens);
            today.messages = today.messages.saturating_add(1);

            let tm = today_models
                .entry(model_name.clone())
                .or_insert_with(|| ModelTokens {
                    model: model_name.clone(),
                    ..Default::default()
                });
            tm.input = tm.input.saturating_add(usage.input_tokens);
            tm.output = tm.output.saturating_add(usage.output_tokens);
            tm.cache_read = tm.cache_read.saturating_add(usage.cache_read_input_tokens);
            tm.cache_create = tm
                .cache_create
                .saturating_add(usage.cache_creation_input_tokens);
        }

        // Historical top-up — only days strictly AFTER last_computed_date, so days
        // already aggregated in the stats-cache are never double-counted.
        if date <= cutoff_date {
            continue;
        }

        let in_out = usage.input_tokens.saturating_add(usage.output_tokens);
        *daily_map.entry(date.to_owned()).or_insert(0) += in_out;

        let entry = model_map
            .entry(model_name.clone())
            .or_insert_with(|| ModelTokens {
                model: model_name,
                ..Default::default()
            });
        entry.input = entry.input.saturating_add(usage.input_tokens);
        entry.output = entry.output.saturating_add(usage.output_tokens);
        entry.cache_read = entry
            .cache_read
            .saturating_add(usage.cache_read_input_tokens);
        entry.cache_create = entry
            .cache_create
            .saturating_add(usage.cache_creation_input_tokens);

        // Track max date added.
        if max_date.as_deref().is_none_or(|prev| date > prev) {
            *max_date = Some(date.to_owned());
        }
    }
}

// ── Background thread ─────────────────────────────────────────────────────────

/// Events emitted by the background loader thread.
pub(crate) enum TokensEvent {
    Loaded(Box<TokenStats>),
    Failed,
}

/// Spawn the token-stats background worker. Loads once on start and sends the
/// result immediately, then loops on `refresh_rx.recv_timeout(REFRESH_INTERVAL)`
/// reloading each time. Exits when `refresh_rx` disconnects (TUI shutdown).
///
/// `claude_dir` must already be resolved by the caller — the worker never
/// re-resolves `home_dir()`, matching the pattern in `status::spawn`.
pub(crate) fn spawn(tx: Sender<TokensEvent>, refresh_rx: Receiver<()>, claude_dir: PathBuf) {
    std::thread::spawn(move || {
        let send = |stats: Option<TokenStats>| {
            let event = match stats {
                Some(s) => TokensEvent::Loaded(Box::new(s)),
                None => TokensEvent::Failed,
            };
            let _ = tx.send(event);
        };

        send(load(&claude_dir));

        loop {
            match refresh_rx.recv_timeout(REFRESH_INTERVAL) {
                Ok(()) => while refresh_rx.try_recv().is_ok() {},
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            send(load(&claude_dir));
        }
    });
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "../tests/inline/tokens.rs"]
mod tests;
