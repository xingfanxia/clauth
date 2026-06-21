//! Pure formatters for the MCP layer: init instructions block, per-call live
//! footer, single usage line, third-party headline. No I/O, no locks — callers
//! pass in already-loaded cache data so these stay unit-testable.

use crate::providers::ThirdPartyStats;
use crate::usage::{UsageWindow, humanize_duration, iso_to_epoch_secs};

/// Per-profile snapshot fed to [`instructions_block`]: identity + the two cached
/// usage windows (standard accounts) or a third-party headline (provider keys).
pub(crate) struct ProfileSnapshot {
    pub(crate) name: String,
    pub(crate) active: bool,
    pub(crate) provider: String,
    pub(crate) base_url: Option<String>,
    pub(crate) sub_type: Option<String>,
    pub(crate) five_h: Option<UsageWindow>,
    pub(crate) seven_d: Option<UsageWindow>,
    pub(crate) third_party: Option<String>,
}

/// Single `<label> <pct>% (resets in <dur>)` line for one usage window. Parses
/// `resets_at` against `now_secs`; on an unparseable timestamp the raw string is
/// shown, and a window with no reset time drops the parenthetical entirely.
pub(crate) fn usage_line(label: &str, w: &UsageWindow, now_secs: i64) -> String {
    let pct = w.utilization.round() as i64;
    match w.resets_at.as_deref() {
        None => format!("{label} {pct}%"),
        Some(raw) => match iso_to_epoch_secs(raw) {
            Some(reset) => {
                format!(
                    "{label} {pct}% (resets in {})",
                    humanize_duration(reset - now_secs)
                )
            }
            None => format!("{label} {pct}% (resets {raw})"),
        },
    }
}

/// One-line cached headline for a third-party profile from
/// `third_party_cache.json`: non-empty bars join as `label pct%`, else the first
/// stat row's text; the plan label prefixes the line when present.
pub(crate) fn third_party_headline(s: &ThirdPartyStats) -> String {
    let body = if !s.bars.is_empty() {
        s.bars
            .iter()
            .map(|b| format!("{} {}%", b.label, b.pct.round() as i64))
            .collect::<Vec<_>>()
            .join(", ")
    } else if let Some(row) = s.rows.first() {
        if row.label.is_empty() {
            row.value.clone()
        } else {
            format!("{}: {}", row.label, row.value)
        }
    } else if !s.is_available {
        "unavailable".to_string()
    } else {
        String::new()
    };

    match (&s.plan, body.is_empty()) {
        (Some(plan), false) => format!("{plan} — {body}"),
        (Some(plan), true) => plan.clone(),
        (None, _) => body,
    }
}

/// Compact freshness footer appended to every `which`/`switch`/`run` result:
/// active profile + 5h/7d headroom for the touched profile, read fresh from cache.
pub(crate) fn live_footer(
    active: Option<&str>,
    five_h: Option<&UsageWindow>,
    seven_d: Option<&UsageWindow>,
) -> String {
    let mut parts = Vec::with_capacity(3);
    if let Some(a) = active {
        parts.push(format!("active={a}"));
    }
    if let Some(w) = five_h {
        parts.push(format!("5h {}%", w.utilization.round() as i64));
    }
    if let Some(w) = seven_d {
        parts.push(format!("7d {}%", w.utilization.round() as i64));
    }
    parts.join(" | ")
}

/// Init-time `instructions` block: policy prose + per-profile inventory and
/// usage snapshot, with the cache age and a "call `list_profiles`" nudge so the
/// model treats every embedded number as a session-start snapshot.
pub(crate) fn instructions_block(
    profiles: &[ProfileSnapshot],
    cache_age_label: &str,
    now_secs: i64,
) -> String {
    let mut out = String::new();
    out.push_str(
        "clauth exposes its account profiles to this session. \
Tools: `list_profiles` (live cache + filesystem, zero quota), \
`which` (resolve this session's active profile), \
`switch` (relink the global active profile — affects the NEXT spawned session, not this one), \
`run` (delegate a headless prompt to a profile; this BURNS a real account usage window). \
`run` is hard-capped at depth 1 (a delegate cannot itself delegate). \
The figures below are a snapshot as of ",
    );
    out.push_str(cache_age_label);
    out.push_str("; call `list_profiles` for live figures.\n\nProfiles:\n");

    for p in profiles {
        out.push_str("- ");
        out.push_str(&p.name);
        if p.active {
            out.push_str(" (active)");
        }
        out.push_str(" [");
        out.push_str(&p.provider);
        if let Some(s) = &p.sub_type {
            out.push_str(", ");
            out.push_str(s);
        }
        if let Some(b) = &p.base_url {
            out.push_str(", ");
            out.push_str(b);
        }
        out.push(']');

        if let Some(tp) = &p.third_party {
            out.push_str(": ");
            out.push_str(tp);
        } else {
            let mut windows = Vec::with_capacity(2);
            if let Some(w) = &p.five_h {
                windows.push(usage_line("5h", w, now_secs));
            }
            if let Some(w) = &p.seven_d {
                windows.push(usage_line("7d", w, now_secs));
            }
            if !windows.is_empty() {
                out.push_str(": ");
                out.push_str(&windows.join(", "));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_render.rs"]
mod tests;
