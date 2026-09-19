//! The one-time fork migration onto upstream's codex file split (UPS-18).
//!
//! Before upstream merged the codex harness (#69), this fork carried the harness
//! as a FIELD: `harness = "codex"` in `profiles/<name>/config.toml`, the codex
//! roster inside `profiles.toml` alongside the claude one, the codex slot and
//! chain as `active_codex_profile` / `codex_fallback_chain` top-level keys, and
//! each codex credential at `profiles/<name>/codex-auth.json`.
//!
//! Upstream's answer makes the harness STRUCTURAL: a profile's harness is which
//! state file holds it — `profiles.toml` for claude, `codex-profiles.toml` for
//! codex — and the codex credential is `profiles/<name>/auth.json`. Nothing reads
//! the legacy shape any more, so on an un-migrated install every codex account
//! simply vanishes from the roster: the new binary loads an empty
//! `codex-profiles.toml` and finds nothing.
//!
//! Nothing is destroyed by that, which is the one piece of luck here.
//! `preserve_unmodelled_state_keys` carries every top-level key `AppState` does
//! not model, so `active_codex_profile` and `codex_fallback_chain` survive a
//! save by the new binary untouched — the legacy record is still on disk when
//! this runs, however many times the daemon has written the file since.
//!
//! This is FORK-ONLY. An install that never ran this fork has nothing here to
//! find, and [`plan`] reports exactly that.
//!
//! # Ordering
//!
//! Every step is idempotent and the order is crash-safe:
//!
//! 1. rename each `codex-auth.json` to `auth.json` (skipped when the
//!    destination already exists — never overwrite a store),
//! 2. write `codex-profiles.toml`,
//! 3. strip the legacy keys from `profiles.toml` and the `harness` key from
//!    every `config.toml`.
//!
//! A crash between any two steps leaves a state that re-running completes. The
//! window between 2 and 3 has each codex name in BOTH rosters, which reads as a
//! duplicate rather than as a loss — the recoverable direction.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::lock::with_state_lock;
use crate::profile::{ProfileName, atomic_write_600, clauth_dir, profile_dir, profile_subpath};

/// The legacy per-profile key naming the harness, in `profiles/<name>/config.toml`.
const LEGACY_HARNESS_KEY: &str = "harness";
/// The legacy per-profile codex credential, beside the claude `credentials.json`.
const LEGACY_STORE: &str = "codex-auth.json";
/// Upstream's per-profile codex credential.
const STORE: &str = "auth.json";
/// The two legacy top-level keys in `profiles.toml`.
const LEGACY_ACTIVE: &str = "active_codex_profile";
const LEGACY_CHAIN: &str = "codex_fallback_chain";

/// What the migration would do, derived entirely from disk. Empty
/// ([`CodexSplitPlan::is_empty`]) on an install that has nothing to migrate,
/// which is the normal case for everyone but this fork's own operator.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct CodexSplitPlan {
    /// Codex profiles in the legacy layout, in `profiles.toml` roster order.
    pub(crate) profiles: Vec<ProfileName>,
    /// The legacy `active_codex_profile`, when set and still a known profile.
    pub(crate) active: Option<ProfileName>,
    /// The legacy `codex_fallback_chain`, filtered to names that still exist.
    pub(crate) chain: Vec<ProfileName>,
    /// `wrap_off` / `weekly_switch_threshold` as the claude state carries them.
    /// The fork had no codex-side twin — one flag and one line governed both
    /// chains — so carrying the claude values across is what preserves the
    /// behaviour the operator configured.
    pub(crate) inherited_wrap_off: bool,
    pub(crate) inherited_weekly: Option<f64>,
    /// Stores to rename, `(from, to)`.
    pub(crate) store_renames: Vec<(PathBuf, PathBuf)>,
    /// `config.toml` files still carrying a `harness` key — claude ones too,
    /// since the key is dead for every harness now.
    pub(crate) harness_keys: Vec<PathBuf>,
    /// Codex names found in the claude `auth_broken` list. Reported, never
    /// translated: upstream records a codex quarantine per profile in
    /// `auth.quarantine.json`, with a verdict and a clock this list does not
    /// carry. Dropping the name is fail-SAFE — an account whose chain really is
    /// dead is re-quarantined by the next poll, on evidence.
    pub(crate) dropped_quarantines: Vec<ProfileName>,
    /// `codex-profiles.toml` held no roster when this was planned, so the run
    /// fills it — slot, chain, wrap-off and weekly line included. When it
    /// already holds one, the run is REPAIRING a partial migration: that file is
    /// authoritative and only the stale claude-roster entries go.
    pub(crate) fills_fresh_codex_state: bool,
    /// `profiles.toml` still carries `active_codex_profile` / `codex_fallback_chain`.
    pub(crate) legacy_keys_present: bool,
}

impl CodexSplitPlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.profiles.is_empty() && self.store_renames.is_empty() && self.harness_keys.is_empty()
    }

    /// One line per action, for `--dry-run` and the doctor note. Names only —
    /// `profiles.toml` can carry an `api_key`, so nothing here prints a value.
    pub(crate) fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.profiles.is_empty() {
            out.push(format!(
                "{} {} codex profile(s): {}",
                if self.fills_fresh_codex_state {
                    "move into codex-profiles.toml"
                } else {
                    "drop from the claude roster"
                },
                self.profiles.len(),
                self.profiles
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(active) = &self.active {
            out.push(format!("carry the active codex slot: {active}"));
        }
        if !self.chain.is_empty() {
            out.push(format!(
                "carry the codex chain: {}",
                self.chain
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(" → ")
            ));
        }
        if self.fills_fresh_codex_state {
            out.push(format!(
                "carry wrap_off={} and the weekly line {} onto the codex chain",
                self.inherited_wrap_off,
                self.inherited_weekly
                    .map_or_else(|| "(default)".to_string(), |w| format!("{w}%"))
            ));
        } else if !self.profiles.is_empty() {
            out.push(
                "codex-profiles.toml already holds them, so its slot, chain and weekly \
                 line stand untouched"
                    .to_string(),
            );
        }
        for (from, _) in &self.store_renames {
            out.push(format!(
                "rename {}/{LEGACY_STORE} → {STORE}",
                from.parent()
                    .and_then(|p| p.file_name())
                    .map_or_else(|| "?".into(), |n| n.to_string_lossy().into_owned())
            ));
        }
        if !self.harness_keys.is_empty() {
            out.push(format!(
                "drop the dead `{LEGACY_HARNESS_KEY}` key from {} config.toml file(s)",
                self.harness_keys.len()
            ));
        }
        if self.legacy_keys_present {
            out.push(format!(
                "drop `{LEGACY_ACTIVE}` and `{LEGACY_CHAIN}` from profiles.toml"
            ));
        }
        for name in &self.dropped_quarantines {
            out.push(format!(
                "drop '{name}' from the claude auth_broken list (a codex quarantine \
                 is a per-profile record now; the next poll re-judges it)"
            ));
        }
        out
    }
}

fn app_state_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("profiles.toml"))
}

fn codex_state_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join("codex-profiles.toml"))
}

/// The `harness` value in `profiles/<name>/config.toml`, when the file carries
/// one. Read as a raw table: `ProfileConfig` no longer models the key, so a
/// typed read cannot see it.
fn legacy_harness_of(name: &ProfileName) -> Option<String> {
    let path = profile_subpath(name, "config.toml").ok()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let table: toml::Table = raw.parse().ok()?;
    table
        .get(LEGACY_HARNESS_KEY)?
        .as_str()
        .map(str::to_ascii_lowercase)
}

/// Read what the migration would do. Pure: touches no file it does not read.
pub(crate) fn plan() -> Result<CodexSplitPlan> {
    let state_path = app_state_path()?;
    if !state_path.exists() {
        return Ok(CodexSplitPlan::default());
    }
    let raw = std::fs::read_to_string(&state_path).context("failed to read profiles.toml")?;
    let table: toml::Table = raw
        .parse()
        .context("profiles.toml does not parse as TOML — refusing to guess at its shape")?;

    let names: Vec<ProfileName> = table
        .get("profiles")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(ProfileName::from)
                .collect()
        })
        .unwrap_or_default();

    let mut plan = CodexSplitPlan {
        inherited_wrap_off: table
            .get("wrap_off")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false),
        inherited_weekly: table
            .get("weekly_switch_threshold")
            .and_then(toml::Value::as_float),
        ..CodexSplitPlan::default()
    };

    // A profile is codex-side if its own config.toml still says so. The legacy
    // chain/slot keys are corroborating evidence, never the classifier: a name
    // in `codex_fallback_chain` whose config.toml says claude would otherwise
    // move a claude account's credentials into the codex roster.
    for name in &names {
        if legacy_harness_of(name).as_deref() == Some("codex") {
            plan.profiles.push(name.clone());
        }
    }

    // Repair: a name the codex roster already holds that the claude roster
    // still lists. That is a partial migration — step 3 stripped the `harness`
    // key but left the name behind — and the classifier above can no longer see
    // it. Names are one namespace across both rosters, so a name in both is
    // always stale on one side; it is dropped from the claude side only on a
    // POSITIVE codex signal on disk (the codex store present, no claude
    // credential), so a real claude account can never be taken for one.
    let codex_now = crate::codex_profiles::CodexState::load().unwrap_or_default();
    plan.fills_fresh_codex_state = codex_now.profiles().is_empty();
    plan.legacy_keys_present =
        table.contains_key(LEGACY_ACTIVE) || table.contains_key(LEGACY_CHAIN);
    for name in &names {
        if plan.profiles.contains(name) || !codex_now.holds(name.as_str()) {
            continue;
        }
        let dir = profile_dir(name)?;
        if dir.join(STORE).exists() && !dir.join("credentials.json").exists() {
            plan.profiles.push(name.clone());
        }
    }

    let known: BTreeSet<&str> = plan.profiles.iter().map(ProfileName::as_str).collect();
    plan.active = table
        .get(LEGACY_ACTIVE)
        .and_then(|v| v.as_str())
        .map(ProfileName::from)
        .filter(|n| known.contains(n.as_str()));
    plan.chain = table
        .get(LEGACY_CHAIN)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(ProfileName::from)
                .filter(|n| known.contains(n.as_str()))
                .collect()
        })
        .unwrap_or_default();
    plan.dropped_quarantines = table
        .get("auth_broken")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(ProfileName::from)
                .filter(|n| known.contains(n.as_str()))
                .collect()
        })
        .unwrap_or_default();

    for name in &plan.profiles {
        let dir = profile_dir(name)?;
        let from = dir.join(LEGACY_STORE);
        let to = dir.join(STORE);
        // Never overwrite an existing store: a half-run migration, or an adopt
        // that already wrote upstream's path, must not lose the live chain.
        if from.exists() && !to.exists() {
            plan.store_renames.push((from, to));
        }
    }

    // The key is dead for EVERY harness, so this sweeps the claude ones too.
    for name in &names {
        if legacy_harness_of(name).is_some() {
            plan.harness_keys
                .push(profile_subpath(name, "config.toml")?);
        }
    }

    Ok(plan)
}

/// Strip one top-level key from a TOML document, preserving everything else
/// byte-for-byte. A `toml::Table` round-trip would reorder and reformat the
/// whole file — and `profiles.toml` is hand-editable, so the diff an operator
/// sees after this must be the keys that actually went.
///
/// Only top-level scalars and arrays are handled, because that is the entire
/// shape of the keys being removed. A key inside a `[table]` is left alone:
/// this walks until the first table header and stops.
fn strip_top_level_key(raw: &str, key: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_target = false;
    // Once a table header opens, every later key belongs to THAT table, not to
    // the document — a `[herdr]` carrying its own `harness` is a different key
    // that happens to share a name, and removing it would be someone else's bug.
    let mut seen_table = false;
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            // A table header ends the top-level scope — and ends any multi-line
            // array we were skipping, which would be a malformed file anyway.
            in_target = false;
            seen_table = true;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if seen_table {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_target {
            // Still inside the removed key's multi-line array value.
            if trimmed.ends_with(']') {
                in_target = false;
            }
            continue;
        }
        let is_target = trimmed
            .split_once('=')
            .is_some_and(|(lhs, _)| lhs.trim() == key);
        if is_target {
            // A multi-line array keeps going until its closing bracket.
            let rhs = trimmed.split_once('=').map(|(_, r)| r.trim()).unwrap_or("");
            if rhs.starts_with('[') && !rhs.ends_with(']') {
                in_target = true;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Remove `names` from a top-level array (`profiles`, `fallback_chain`,
/// `auth_broken`) while leaving the rest of the file alone.
///
/// The value may span lines: clauth renders its own state with
/// `to_string_pretty`, which puts every array of more than one element on its
/// own lines. The first cut of this handled single-line arrays only and left a
/// multi-line one untouched — which on a real `profiles.toml` left every codex
/// name in the claude roster while the rest of the migration succeeded.
fn remove_from_array(raw: &str, key: &str, names: &BTreeSet<String>) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let mut out = String::with_capacity(raw.len());
    let mut seen_table = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') && !trimmed.contains('=') {
            seen_table = true;
        }
        let is_target = !seen_table
            && trimmed
                .split_once('=')
                .is_some_and(|(lhs, _)| lhs.trim() == key);
        if !is_target {
            out.push_str(line);
            out.push('\n');
            i += 1;
            continue;
        }
        // Take the whole value: an array runs until its brackets balance.
        // Profile names are charset-validated, so no bracket hides in a string.
        let mut end = i;
        let mut depth = 0i32;
        loop {
            for c in lines[end].chars() {
                match c {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
            }
            if depth <= 0 || end + 1 >= lines.len() {
                break;
            }
            end += 1;
        }
        let block = lines[i..=end].join("\n");
        match block
            .parse::<toml::Table>()
            .ok()
            .and_then(|t| t.get(key).cloned())
        {
            Some(toml::Value::Array(items)) => {
                let kept: Vec<toml::Value> = items
                    .into_iter()
                    .filter(|v| v.as_str().is_none_or(|s| !names.contains(s)))
                    .collect();
                out.push_str(&format!("{key} = {}\n", toml::Value::Array(kept)));
            }
            // Not an array, or not parseable on its own: leave it exactly as
            // found rather than corrupt the document.
            _ => {
                out.push_str(&block);
                out.push('\n');
            }
        }
        i = end + 1;
    }
    out
}

/// Run the migration. Takes the state lock for the whole run, so no daemon tick
/// can read a half-split roster.
///
/// Refuses rather than guesses when `codex-profiles.toml` already holds a
/// roster AND the legacy layout is also present: that is two sources of truth
/// for the same accounts, and picking one silently is how an operator loses the
/// chain they actually configured.
pub(crate) fn run(plan: &CodexSplitPlan) -> Result<()> {
    if plan.is_empty() {
        return Ok(());
    }
    with_state_lock(|_held| {
        let codex_path = codex_state_path()?;
        let existing = crate::codex_profiles::CodexState::load().unwrap_or_default();
        if !existing.profiles().is_empty() {
            let already: BTreeSet<&str> = existing.profiles().iter().map(|n| n.as_str()).collect();
            if let Some(clash) = plan.profiles.iter().find(|n| !already.contains(n.as_str())) {
                bail!(
                    "codex-profiles.toml already holds a roster that does not include \
                     '{clash}'. Two rosters describe these accounts and this refuses to \
                     pick one — reconcile them by hand, then re-run."
                );
            }
        }

        // 1. Stores first: a rename that lands with nothing else done is simply
        //    a credential at its new path, which the next run finds already there.
        for (from, to) in &plan.store_renames {
            std::fs::rename(from, to)
                .with_context(|| format!("failed to rename {}", from.display()))?;
        }

        // 2. The codex roster. Rendered through CodexState so the file this
        //    writes is byte-identical to one the daemon would write itself.
        crate::codex_profiles::CodexState::update(|state| {
            // Judged under the lock, not from the plan: only a FRESH codex state
            // takes the legacy settings. An existing one is authoritative — this
            // run is repairing a partial migration whose legacy keys are already
            // gone, and carrying them would reset its active slot and chain to
            // empty.
            let fresh = state.profiles().is_empty();
            for name in &plan.profiles {
                state.add_profile(name.as_str());
            }
            if fresh {
                *state.fallback_chain_mut() = plan.chain.clone();
                state.set_active(plan.active.as_ref().map(|n| n.as_str()));
                state.set_switch_off_when_spent(plan.inherited_wrap_off);
                state.set_weekly_switch_threshold(plan.inherited_weekly);
            }
            Ok(())
        })
        .with_context(|| format!("failed to write {}", codex_path.display()))?;

        // 3. The legacy record, last. Edited as text, not round-tripped: the
        //    file is hand-editable and an operator's next diff should show the
        //    keys that went, not a wholesale reformat.
        let state_path = app_state_path()?;
        let raw = std::fs::read_to_string(&state_path).context("failed to read profiles.toml")?;
        let names: BTreeSet<String> = plan.profiles.iter().map(|n| n.to_string()).collect();
        let mut edited = strip_top_level_key(&raw, LEGACY_ACTIVE);
        edited = strip_top_level_key(&edited, LEGACY_CHAIN);
        for key in ["profiles", "fallback_chain", "auth_broken"] {
            edited = remove_from_array(&edited, key, &names);
        }
        // A parse check before the write: this edits text, so it proves the
        // result is still a document clauth can load before replacing the one
        // that demonstrably was.
        let edited_table: toml::Table = edited
            .parse()
            .context("the migrated profiles.toml does not parse — refusing to write it")?;
        // The invariant this whole migration exists to establish: no name in
        // both rosters. Checked on the result rather than trusted from the edit,
        // because an edit that silently misses (a multi-line array did, once)
        // otherwise reports success over a half-split file.
        let still_listed: Vec<&str> = edited_table
            .get("profiles")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|n| names.contains(*n))
                    .collect()
            })
            .unwrap_or_default();
        if !still_listed.is_empty() {
            bail!(
                "the edit left {} in the claude roster as well as the codex one — \
                 refusing to write a half-split profiles.toml",
                still_listed.join(", ")
            );
        }
        atomic_write_600(&state_path, &edited).context("failed to write profiles.toml")?;

        for path in &plan.harness_keys {
            strip_harness_key(path)?;
        }
        Ok(())
    })
}

/// Drop the dead `harness` key from one `profiles/<name>/config.toml`.
fn strip_harness_key(path: &Path) -> Result<()> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(()); // vanished under us: nothing to strip
    };
    let edited = strip_top_level_key(&raw, LEGACY_HARNESS_KEY);
    if edited == raw {
        return Ok(());
    }
    edited
        .parse::<toml::Table>()
        .with_context(|| format!("{} would not parse after the edit", path.display()))?;
    atomic_write_600(path, &edited)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/inline/migrate_codex_split.rs"]
mod tests;
