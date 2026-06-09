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

/// Compute burn rates (%/h) per usage window from cached history plus the
/// latest current usage.
///
/// `windows` is a slice of `(label, &UsageWindow)` pairs — typically a subset
/// of [`UsageInfo::windows`]. Each window gets its own `(min_entries,
/// min_span_ms)` so that 5-hour windows can use a narrow rolling window
/// (e.g. 5 entries / 30 min) while 7-day windows use a wider one
/// (e.g. 10 entries / 6 h) to avoid burst extrapolation.
///
/// Returns rates in %/h — multiply by 24 for %/d display.  
pub(crate) fn compute_burn_rates_from_history(
    history: &[(u64, UsageInfo)],
    windows: &[(&str, &UsageWindow)],
    min_entries: usize,
    min_span_ms: u64,
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

        if entries.len() >= 2 {
            entries.dedup_by(|a, b| a.1 == b.1);
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
