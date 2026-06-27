//! Pure formatters for the MCP layer: init instructions block, per-call live
//! footer, single usage line, third-party headline. No I/O, no locks — callers
//! pass in already-loaded cache data so these stay unit-testable.

use crate::providers::ThirdPartyStats;
use crate::usage::UsageWindow;
use crate::which::SessionAuth;

/// Per-profile snapshot fed to [`instructions_block`]: stable identity only (name,
/// provider, tier, base url). Volatile usage figures rot within a turn, so they are
/// served fresh per call by `list_profiles`, never baked into the boot-time block.
pub(crate) struct ProfileSnapshot {
    pub(crate) name: String,
    pub(crate) active: bool,
    pub(crate) provider: String,
    pub(crate) base_url: Option<String>,
    pub(crate) sub_type: Option<String>,
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
/// `switch` note, the `delegate` cost model, then the per-profile roster. Only
/// stable facts are baked in — usage percentages and reset timers rot within a turn,
/// so they live in `list_profiles` (read fresh per call), not here. The roster
/// itself is a session-start snapshot.
pub(crate) fn instructions_block(profiles: &[ProfileSnapshot], auth: &SessionAuth) -> String {
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
        "\n\nCost: `delegate` to a subscription profile burns a rate-limited window (no per-token \
charge); to an API-key profile (DeepSeek, Z.ai) it bills real USD; a local endpoint is free. To \
pick the cheapest target, call `list_profiles` for live windows + third-party balances.\n\n\
A delegate sees nothing but the prompt you pass it — frame the task in that prompt; it has no view \
of this conversation.\n\n\
Profiles (at session start — call `list_profiles` for the live roster and usage):\n",
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
        out.push_str("]\n");
    }
    out
}

#[cfg(test)]
#[path = "../../tests/inline/mcp_render.rs"]
mod tests;
