use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::usage::{LABEL_5H, LABEL_7D, LABEL_7D_OPUS, LABEL_7D_SONNET, UsageInfo, UsageWindow};

fn window_util(usage: &UsageInfo, label: &str) -> Option<f64> {
    match label {
        LABEL_5H => usage.five_hour.as_ref().map(|w| w.utilization),
        LABEL_7D => usage.seven_day.as_ref().map(|w| w.utilization),
        LABEL_7D_SONNET => usage.seven_day_sonnet.as_ref().map(|w| w.utilization),
        LABEL_7D_OPUS => usage.seven_day_opus.as_ref().map(|w| w.utilization),
        _ => None,
    }
}

/// Walk `entries` (chronological) to find the index of the first entry after
/// the most recent utilization drop (window reset). Returns 0 when no reset is
/// detected.
fn reset_boundary(entries: &[(u64, f64)]) -> usize {
    for i in (1..entries.len()).rev() {
        if entries[i].1 < entries[i - 1].1 {
            return i;
        }
    }
    0
}

/// Walk `entries` (chronological) to find the most recent pair of consecutive
/// entries with identical utilization whose time gap exceeds `max_gap_ms`.
/// Returns the index of the later entry (start of the active period after an
/// idle stretch), or 0 when no idle gap is detected.
fn gap_boundary(entries: &[(u64, f64)], max_gap_ms: u64) -> usize {
    for i in (1..entries.len()).rev() {
        if entries[i].1 == entries[i - 1].1
            && entries[i].0.saturating_sub(entries[i - 1].0) > max_gap_ms
        {
            return i;
        }
    }
    0
}

/// Compute burn rates (%/h) per usage window from cached history plus the
/// latest current usage.
///
/// `windows` is a slice of `(label, &UsageWindow)` pairs — typically a subset
/// of [`UsageInfo::windows`]. Each window gets its own `(min_entries,
/// min_span_ms)` so that 5-hour windows can use a narrow rolling window
/// (e.g. 5 entries / 30 min) while 7-day windows use a wider one
/// (e.g. 50 entries / 24 h) to avoid burst extrapolation.
///
/// `gap_cut_ms` controls idle-gap detection: when two consecutive entries
/// share the same utilization and their timestamps differ by more than
/// `gap_cut_ms`, the history is sliced from the later entry onward. Pass 0
/// to disable gap-cut entirely (appropriate for 7-day windows where
/// overnight idle is part of the average).
///
/// Returns rates in %/h — multiply by 24 for %/d display.  
pub(crate) fn compute_burn_rates_from_history(
    history: &[(u64, UsageInfo)],
    windows: &[(&str, &UsageWindow)],
    min_entries: usize,
    min_span_ms: u64,
    gap_cut_ms: u64,
) -> HashMap<String, Option<f64>> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let mut rates = HashMap::new();
    for (label, window) in windows {
        let mut entries: Vec<(u64, f64)> = history
            .iter()
            .filter_map(|(ts, u)| window_util(u, label).map(|p| (*ts, p)))
            .collect();

        entries.push((now_ms, window.utilization));

        // Gap-cut: slice off any idle stretch before the current activity.
        // Must run before dedup — the equal-util pair marks the gap.
        if gap_cut_ms > 0 && entries.len() >= 2 {
            let gap = gap_boundary(&entries, gap_cut_ms);
            if gap > 0 {
                entries = entries[gap..].to_vec();
            }
        }

        if entries.len() >= 2 {
            entries.reverse();
            entries.dedup_by(|a, b| a.1 == b.1);
            entries.reverse();
        }

        let window = &entries[reset_boundary(&entries)..];

        if window.len() >= 2 {
            let n = window.len();
            let last_ts = window[n - 1].0;

            // Span-first: try to cover min_span_ms of data for a stable rate.
            // Fall back to min_entries when the history isn't long enough yet
            // (e.g. early after a window reset).
            let start_idx = (0..n - 1)
                .rev()
                .find(|&i| last_ts - window[i].0 >= min_span_ms)
                .or_else(|| (0..n - 1).rev().find(|&i| n - i >= min_entries))
                .unwrap_or(0);

            let first = &window[start_idx];
            let last = &window[n - 1];
            let dt = (last.0 - first.0) as f64 / 3_600_000.0;
            if dt > 0.0 {
                let rate = (last.1 - first.1) / dt;
                rates.insert(label.to_string(), Some(rate));
                continue;
            }
        }
        rates.insert(label.to_string(), None);
    }
    rates
}

#[cfg(test)]
#[path = "../../tests/inline/burn.rs"]
mod tests;
