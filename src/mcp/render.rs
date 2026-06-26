//! Pure formatters for the MCP layer: init instructions block, per-call live
//! footer, single usage line, third-party headline. No I/O, no locks — callers
//! pass in already-loaded cache data so these stay unit-testable.

use crate::providers::ThirdPartyStats;
use crate::usage::{UsageWindow, humanize_duration, iso_to_epoch_secs};
use crate::which::SessionAuth;

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
        None => format!("{label} {pct}% used"),
        Some(raw) => match iso_to_epoch_secs(raw) {
            Some(reset) => {
                format!(
                    "{label} {pct}% used (resets in {})",
                    humanize_duration(reset - now_secs)
                )
            }
            None => format!("{label} {pct}% used (resets {raw})"),
        },
    }
}

/// One-line cached headline for a third-party profile from
/// `third_party_cache.json`: non-empty bars join as `label pct%`, else the first
/// stat row that carries a value; the plan label prefixes the line when present.
/// Value-less rows (e.g. DeepSeek's `USD balance` heading) are skipped so the
/// headline never renders a dangling `label:` with nothing after it.
pub(crate) fn third_party_headline(s: &ThirdPartyStats) -> String {
    let body = if !s.bars.is_empty() {
        s.bars
            .iter()
            .map(|b| format!("{} {}%", b.label, b.pct.round() as i64))
            .collect::<Vec<_>>()
            .join(", ")
    } else if let Some(row) = s.rows.iter().find(|r| !r.value.is_empty()) {
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

/// Compact freshness footer appended to every `which`/`switch`/`delegate` result:
/// active profile + 5h/7d percent-used for the touched profile, read fresh from
/// cache. Percentages are the share of the window consumed (higher = less
/// headroom), labeled `% used` so the reader can't invert it.
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
        parts.push(format!("5h {}% used", w.utilization.round() as i64));
    }
    if let Some(w) = seven_d {
        parts.push(format!("7d {}% used", w.utilization.round() as i64));
    }
    parts.join(" | ")
}

/// What a `switch` does to *this* session, keyed on how it reads its credentials.
/// A global session reads the exact file `switch` repoints; an isolated session
/// (a `clauth start` runtime or a custom `CLAUDE_CONFIG_DIR`) reads its own, so a
/// switch can't disturb it. Pure mapping — the caller resolves the [`SessionAuth`].
pub(crate) fn switch_effect(auth: &SessionAuth) -> String {
    match auth {
        SessionAuth::Global => "`switch` repoints the global `~/.claude` credentials THIS \
session reads; Claude Code reloads them on its next token refresh, so this session would \
start acting as the switched profile — disruptive mid-session. To use another account \
without disturbing this one, use the `delegate` tool."
            .to_string(),
        SessionAuth::IsolatedRuntime(name) => format!(
            "`switch` repoints the global `~/.claude` credentials, but THIS session runs in an \
isolated `clauth start` runtime pinned to `{name}` and is unaffected — only a later session on \
the global credentials adopts the change."
        ),
        SessionAuth::IsolatedCustom => "`switch` repoints the global `~/.claude` credentials, but \
THIS session uses a custom `CLAUDE_CONFIG_DIR` and reads its own credentials, so it is \
unaffected — only a later session on the global credentials adopts the change."
            .to_string(),
    }
}

/// Init-time `instructions` block: identity + when-to-use intro, a session-aware
/// `switch` note, then the per-profile inventory and usage snapshot, with the
/// cache age and a "call `list_profiles`" nudge so the model treats every embedded
/// number as a session-start snapshot.
pub(crate) fn instructions_block(
    profiles: &[ProfileSnapshot],
    auth: &SessionAuth,
    cache_age_label: &str,
    now_secs: i64,
) -> String {
    let mut out = String::new();
    out.push_str(
        "clauth manages multiple Claude Code accounts (\"profiles\") — each an isolated \
credential set / subscription. Use these tools to compare usage headroom across accounts, \
relink the active account, or delegate a task to another account without spending this \
session's window.\n\n\
Tools: `list_profiles` (cached usage + filesystem, zero quota), \
`which` (the profile that owns this session's credentials), \
`switch` (relink the global active profile), \
`delegate` (delegate a headless prompt to a profile; this BURNS a real account usage window, \
hard-capped at depth 1 — a delegate cannot itself delegate; pass `background:true` for a `job_id` \
now and the result later), \
`delegate_result` (fetch a backgrounded delegate's result by `job_id`).\n\nswitch & this session: ",
    );
    out.push_str(&switch_effect(auth));
    out.push_str(
        "\n\nUsage percentages are the share of each window already used \
(higher = less headroom). These figures are cached snapshots (active profile cached ",
    );
    out.push_str(cache_age_label);
    out.push_str(
        "; other profiles may be staler); call `list_profiles` for live figures.\n\nProfiles:\n",
    );

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
