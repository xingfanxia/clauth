//! Pure profile/usage → display-string formatters, plus the cross-surface
//! diagnostic messages. No UI dependencies, so the TUI, the CLI subcommands
//! (e.g. `clauth which`), and the headless daemon all share one spelling.

use crate::profile::Profile;
use crate::usage::{PlanInfo, PlanTier};

// ── Cross-surface diagnostics ───────────────────────────────────────────────
//
// A condition that surfaces on more than one surface — a CLI `bail!`, a daemon
// `logline!`, a TUI toast — is worded here once. Each surface used to spell the
// same event its own way and they drifted (one condition printed four different
// sentences). `head` is the at-a-glance summary; `detail` the cause and the
// recovery step.

/// One diagnostic, rendered per surface. Keep `head` short enough to read on a
/// toast's bold first line without wrapping; put the cause and next step in
/// `detail`.
pub(crate) struct Message {
    head: String,
    detail: Option<String>,
}

impl Message {
    /// Single-line form for a CLI `bail!` or a `logline!` body (`head: detail`).
    /// The caller prepends any `clauth `/`clauth daemon: ` log prefix.
    pub(crate) fn line(&self) -> String {
        match &self.detail {
            Some(d) => format!("{}: {}", self.head, d),
            None => self.head.clone(),
        }
    }

    /// Toast form: `head` on its own line, `detail` below it. The toast renderer
    /// styles line 1 bold and the rest dim, so the split reads as summary + note.
    pub(crate) fn toast(&self) -> String {
        match &self.detail {
            Some(d) => format!("{}\n{}", self.head, d),
            None => self.head.clone(),
        }
    }
}

/// A login whose refresh token is dead: re-login is the only fix. Shared by the
/// CLI/MCP switch bail, the daemon tick log, and the TUI switch toast.
pub(crate) fn login_expired(name: &str) -> Message {
    Message {
        head: format!("login for '{name}' has expired"),
        detail: Some(format!(
            "refresh token revoked or invalid: run clauth login {name}"
        )),
    }
}

/// A refresh that failed for a transient reason (network): this switch is
/// refused but the login is not quarantined. Retry is the fix.
pub(crate) fn refresh_transient(name: &str, err: &str) -> Message {
    Message {
        head: format!("could not refresh '{name}' before switching"),
        detail: Some(format!("{err}: check your connection and retry")),
    }
}

/// The one spelling for "go fix this in the app". The surface is the `clauth`
/// TUI, never a bare "the TUI" (which reads as some other UI).
pub(crate) const RESOLVE_IN_TUI: &str = "resolve the divergence in the clauth TUI";

/// Trailing-ellipsis truncation to `max` chars (counts `char`s, not bytes).
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub(crate) fn endpoint_label(profile: &Profile) -> String {
    if let Some(url) = &profile.base_url {
        return url.clone();
    }
    if let Some(plan) = profile.usage.as_ref().and_then(|u| u.plan.as_ref()) {
        return plan_label(plan);
    }
    // No fetched plan yet — fall back to the OAuth token's subscription_type.
    let sub = profile
        .credentials
        .as_ref()
        .and_then(|c| c.claude_ai_oauth.as_ref())
        .and_then(|o| o.subscription_type.as_deref());
    PlanTier::from_subscription_type(sub).display()
}

pub(crate) fn plan_label(plan: &PlanInfo) -> String {
    plan.tier.display()
}

#[cfg(test)]
#[path = "../tests/inline/format.rs"]
mod tests;
