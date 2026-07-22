//! Pure-data mutations against `AppConfig` and the live `~/.claude` state.
//!
//! Each function takes already-validated inputs from the TUI layer and applies
//! the change under the cross-process state lock.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::claude::{
    ClaudeEndpoint, LinkState, apply_profile_to_claude_settings, classify_credentials_link,
    clear_claude_credentials, force_link_profile_credentials, force_snapshot_active_credentials,
    is_first_login, link_profile_credentials, live_credentials_are_shell,
    live_diverged_and_unsaved, managed_env_key_label, read_claude_credentials,
    read_claude_endpoint_config, snapshot_active_credentials,
};
use crate::lock::with_state_lock;
use crate::lockorder::RankedMutex;
use crate::oauth;
use crate::profile::{
    AppConfig, ClaudeCredentials, DivergenceChoice, ModelSettings, Profile, profile_dir,
    save_app_state, save_profile,
};
use crate::providers::Provider;
use crate::spinner::Spinner;

/// ASCII alphanumeric + `-_.@+`, not leading-dot, not empty, not a duplicate
/// (`exclude` exempts the current name for rename-in-place). `@`/`+` let an
/// account be named after its email; both are path-separator-free so the name
/// stays a single `profiles/<name>` segment with no traversal.
pub(crate) fn validate_profile_name(
    name: &str,
    existing: &[&str],
    exclude: Option<&str>,
) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("name cannot be empty");
    }
    let valid_chars = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '+'));
    if !valid_chars || trimmed.starts_with('.') {
        bail!("name: letters, digits and - _ . @ + only, and can't start with '.'");
    }
    if existing
        .iter()
        .any(|&n| n.eq_ignore_ascii_case(trimmed) && Some(n) != exclude)
    {
        bail!("a profile named '{trimmed}' already exists");
    }
    Ok(())
}

/// Every switch primitive tears the live credentials link down before
/// `finish_switch` would notice a ghost, and the discard path takes no prior
/// snapshot — an uncaptured re-login would be gone for good. So existence
/// FIRST: a caller holding a stale name (a queued auto-switch target, the MCP
/// switch tool with a divergence default) bounces off before any side effect
/// instead of stranding the machine half-switched with the live link destroyed.
fn ensure_profile_exists(config: &AppConfig, name: &str) -> Result<()> {
    if config.find(name).is_none() {
        bail!("profile '{name}' not found");
    }
    Ok(())
}

pub(crate) fn switch_profile(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
        ensure_profile_exists(config, name)?;
        if config.is_active(name) {
            return Ok(());
        }
        // Is the outgoing live file an UNCAPTURED CC re-login? `snapshot_active_
        // credentials` deliberately skips capturing that case (Diverged & not a
        // first-login), so dropping it would strand a fresh `/login` chain — keep
        // the non-force refuse-guard there. Every other state is captured or
        // adoptable by the snapshot below, so force the relink: on macOS the live
        // `.credentials.json` is always a regular-file Keychain mirror of the
        // active account and thus legitimately differs from the target, which the
        // non-force guard's live-vs-target byte check would wrongly reject on every
        // switch. (Interactive callers already route a real divergence to the
        // reconcile path, so this branch is only reachable uncaptured via the
        // scheduler — where refusing, not dropping, is the safe outcome.)
        // A logged-out shell holds no login to strand, so it forfeits the
        // refuse-guard (which would otherwise wedge the switch on an empty file).
        let uncaptured_relogin = match config.state.active_profile.as_deref() {
            Some(active) => {
                matches!(classify_credentials_link(active)?, LinkState::Diverged)
                    && !is_first_login(active)?
                    && !live_credentials_are_shell()
            }
            None => false,
        };
        snapshot_active_credentials(config)?;
        if uncaptured_relogin {
            link_profile_credentials(name)?;
        } else {
            force_link_profile_credentials(name)?;
        }
        finish_switch(config, name)
    })
}

/// Discard the live login: force-relink to `target`'s stored creds WITHOUT
/// capturing the foreign live file into any profile. Bypasses the non-force
/// `link_profile_credentials` refuse-guard (which exists to protect an
/// un-captured re-login) precisely because the caller chose to drop it.
pub(crate) fn switch_profile_discard(config: &mut AppConfig, target: &str) -> Result<()> {
    with_state_lock(|| {
        ensure_profile_exists(config, target)?;
        if config.is_active(target) {
            return Ok(());
        }
        force_link_profile_credentials(target)?;
        finish_switch(config, target)
    })
}

/// Force-snapshot the outgoing creds then force the symlink. CLI prompt path only.
pub(crate) fn switch_profile_reconciled(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
        ensure_profile_exists(config, name)?;
        if config.is_active(name) {
            return Ok(());
        }
        force_snapshot_active_credentials(config)?;
        force_link_profile_credentials(name)?;
        finish_switch(config, name)
    })
}

/// CLI switch: relink (reconciling diverged live file via `[Y/n]` prompt), then
/// prime the 5h window. No token rotation — stale chains rotate lazily on first use.
pub(crate) fn switch_profile_cli(config: AppConfig, canonical: &str) -> Result<()> {
    let outgoing = config.state.active_profile.clone();

    // Diverged link = CC re-logged and wrote a regular file; must reconcile
    // (capture into outgoing profile) rather than refuse. A logged-out shell is
    // exempt: capturing its blank tokens would destroy the outgoing profile's
    // stored login.
    let reconciled = match outgoing.as_deref() {
        Some(active) => live_diverged_and_unsaved(active)?,
        None => false,
    };

    let config = Arc::new(RankedMutex::new(config));

    // AUTH-1 (Incident C): gate the target before its credentials land in the
    // Keychain (which re-authenticates every running `claude` on this machine).
    // Refusal + `clauth login` hint pinned by
    // `switch_cli_refuses_dead_target_with_login_hint`.
    // The already-active profile is exempt: there is nothing new to install
    // (`switch_profile` no-ops on `is_active`), and its chain is the one a
    // plain `claude` may be refreshing through the symlink right now — gating
    // it can lose that race and false-quarantine a healthy login.
    if outgoing.as_deref() != Some(canonical) {
        match oauth::ensure_installable(&config, canonical, oauth::refresh_result) {
            oauth::AuthGate::Ready | oauth::AuthGate::Refreshed => {}
            oauth::AuthGate::Broken => bail!("{}", crate::format::login_expired(canonical).line()),
            oauth::AuthGate::Transient(e) => {
                bail!(
                    "{}",
                    crate::format::refresh_transient(canonical, &e.to_string()).line()
                )
            }
        }
    }

    if reconciled {
        let active = {
            #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            let cfg = config.lock().expect("config mutex poisoned");
            cfg.state
                .active_profile
                .as_deref()
                .unwrap_or("")
                .to_string()
        };
        print!(
            "clauth: '{active}' has a newer login in ~/.claude. save it into '{active}' \
             and switch to '{canonical}'? [Y/n] "
        );
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer.is_empty() || answer == "y" || answer == "yes" {
            #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            let mut cfg = config.lock().expect("config mutex poisoned");
            switch_profile_reconciled(&mut cfg, canonical)?;
        } else {
            println!("clauth: aborted, no changes made");
            return Ok(());
        }
    } else {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let mut cfg = config.lock().expect("config mutex poisoned");
        switch_profile(&mut cfg, canonical)?;
    }

    // Prime the 5h window if opted in. Kicks with the current access token and
    // rotates once on a 401/429. One-shot — the CLI has no scheduler tick to
    // re-arm against, so no side channels.
    {
        let _spinner = Spinner::start("clauth: priming usage window");
        let _ = oauth::prime_window(&config, canonical);
    }
    println!("clauth: switched to '{canonical}'");
    Ok(())
}

/// Headless switch for the MCP `switch` tool: relink the global active profile
/// to `target` without prompting and without priming the 5h window (zero quota;
/// the profile primes its own window when a session next uses it).
///
/// On credential divergence (the active link is a regular file CC re-logged into)
/// the caller-supplied `on_divergence` decides: `Overwrite` captures the live
/// tokens into the outgoing profile then relinks ([`switch_profile_reconciled`]),
/// `Discard` drops the foreign live login and force-relinks `target`'s stored
/// tokens without capturing it into any profile ([`switch_profile_discard`]),
/// `NewProfile` is interactive-only (would need a name prompt) so it errors, and
/// `None` means no default is set so it errors. A non-diverged link
/// (`LinkedTo`/`Missing`) always takes the plain [`switch_profile`].
///
/// Returns `(previous_active, new_active)`.
///
/// Accepted TOCTOU: the divergence classify runs before the locked relink (same
/// shape as the CLI path); a live change in that gap self-heals on the next switch.
///
/// Takes the shared [`crate::profile::ConfigHandle`] (not `&mut AppConfig`)
/// because the AUTH-1 gate below may refresh over HTTP, which must never run
/// under the config mutex. `refresher` is injected so the gate is testable
/// offline (production callers pass [`oauth::refresh_result`]).
pub(crate) fn switch_profile_noninteractive(
    config: &crate::profile::ConfigHandle,
    target: &str,
    on_divergence: Option<DivergenceChoice>,
    refresher: impl Fn(
        &str,
        Option<&str>,
    ) -> std::result::Result<oauth::TokenResponse, oauth::RefreshError>,
) -> Result<(Option<String>, String)> {
    let previous = {
        #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let cfg = config.lock().expect("config mutex poisoned");
        cfg.state.active_profile.as_deref().map(str::to_string)
    };

    // AUTH-1 (Incident C): gate the target before its credentials land in the
    // Keychain — the same gate as the CLI switch, so "a quarantined account is
    // refused as a switch target" holds for EVERY noninteractive entry point
    // (MCP today; any future headless caller inherits it).
    // The already-active profile is exempt for the same reason as the CLI
    // path: nothing new to install, and gating it races a plain `claude`
    // refreshing the symlinked live file (a lost race false-quarantines).
    if previous.as_deref() != Some(target) {
        match oauth::ensure_installable(config, target, refresher) {
            oauth::AuthGate::Ready | oauth::AuthGate::Refreshed => {}
            oauth::AuthGate::Broken => bail!("{}", crate::format::login_expired(target).line()),
            oauth::AuthGate::Transient(e) => {
                bail!(
                    "{}",
                    crate::format::refresh_transient(target, &e.to_string()).line()
                )
            }
        }
    }

    // A logged-out shell is no divergence to resolve: skip the default and take
    // the plain switch, which replaces the empty file.
    let diverged = match previous.as_deref() {
        Some(active) => live_diverged_and_unsaved(active)?,
        None => false,
    };

    #[allow(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
    let config = &mut *config.lock().expect("config mutex poisoned");
    if diverged {
        match on_divergence {
            Some(DivergenceChoice::Overwrite) => switch_profile_reconciled(config, target)?,
            Some(DivergenceChoice::Discard) => switch_profile_discard(config, target)?,
            Some(DivergenceChoice::NewProfile) | None => {
                let active = previous.as_deref().unwrap_or_default();
                bail!(
                    "'{active}' has a login clauth hasn't saved, {}",
                    crate::format::RESOLVE_IN_TUI
                )
            }
        }
    } else {
        switch_profile(config, target)?;
    }

    Ok((previous, target.to_string()))
}

/// Snapshot active creds then clear them so Claude Code can't spend any account.
/// Used by wrap-off mode when the whole chain is exhausted. No-op when no profile
/// is active. A diverged live file is cleared WITHOUT being snapshotted
/// (`snapshot_active_credentials` skips it, keeping the stored identity), so a
/// fresh `/login` is dropped: the TUI gates that on the divergence prompt, while
/// the automatic wrap-off leg accepts the drop, unattended by design.
pub(crate) fn switch_off(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|| {
        if config.state.active_profile.is_none() {
            return Ok(());
        }
        snapshot_active_credentials(config)?;
        clear_claude_credentials()?;
        // No active account left to show; issue #17 applies here too — a
        // stale identity block is just as wrong once creds are cleared.
        crate::claude_json::strip_home_oauth_account()?;
        config.state.active_profile = None;
        save_app_state(&config.state)
    })
}

fn finish_switch(config: &mut AppConfig, name: &str) -> Result<()> {
    // Capture outgoing env keys before active_profile is reassigned.
    let prev_env_keys: Vec<String> = config
        .state
        .active_profile
        .as_deref()
        .and_then(|n| config.find(n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_default();
    let profile = config.find(name).context("profile not found")?;
    apply_profile_to_claude_settings(profile, &prev_env_keys)?;
    // issue #17: drop the outgoing account's cached identity so Claude Code
    // re-derives it from the just-relinked credentials instead of showing
    // the wrong account until its next `/login`.
    crate::claude_json::strip_home_oauth_account()?;
    config.state.active_profile = Some(name.into());
    save_app_state(&config.state)
}

pub(crate) fn edit_profile_endpoint(
    config: &mut AppConfig,
    name: &str,
    base_url: Option<String>,
    api_key: Option<String>,
) -> Result<()> {
    with_state_lock(|| {
        let profile = config.find_mut(name).context("profile not found")?;
        let old_api_key = profile.api_key.clone();
        profile.base_url = base_url;
        profile.api_key = api_key;
        // Re-derive the provider — the in-memory config is authoritative until
        // the next disk reload, so a stale value here would keep (or block)
        // third-party fetches against the wrong endpoint. Also clear when only the
        // api key changed for the same provider (rotated key — old stats are stale).
        let provider = profile
            .base_url
            .as_deref()
            .and_then(crate::providers::Provider::from_base_url);
        if provider != profile.provider || (provider.is_some() && profile.api_key != old_api_key) {
            profile.third_party_usage = None;
        }
        profile.provider = provider;
        save_profile(profile)?;

        if config.is_active(name) {
            let profile = config.find(name).context("profile not found")?;
            let prev_env_keys: Vec<String> = profile.env.keys().cloned().collect();
            apply_profile_to_claude_settings(profile, &prev_env_keys)?;
        }
        Ok(())
    })
}

/// Persist a profile's model configuration. Re-applies to the live
/// `~/.claude/settings.json` when the profile is active so a running `claude`
/// picks it up on its next settings read. Mirrors [`edit_profile_endpoint`].
pub(crate) fn edit_profile_model(
    config: &mut AppConfig,
    name: &str,
    models: ModelSettings,
) -> Result<()> {
    with_state_lock(|| {
        let profile = config.find_mut(name).context("profile not found")?;
        profile.models = models;
        save_profile(profile)?;

        if config.is_active(name) {
            // A model-only edit never touches the generic `env` map, so passing
            // this profile's own keys as `prev` strips nothing (the removal loop
            // keeps every key the profile still carries). The model env keys
            // (`ANTHROPIC_DEFAULT_*`/`CLAUDE_CODE_SUBAGENT_MODEL`) are set or
            // cleared unconditionally inside `build_claude_settings_json`.
            let profile = config.find(name).context("profile not found")?;
            let prev_env_keys: Vec<String> = profile.env.keys().cloned().collect();
            apply_profile_to_claude_settings(profile, &prev_env_keys)?;
        }
        Ok(())
    })
}

/// Persist a profile's custom env map (the Setup-tab field editor). Captures the
/// OLD env keys first so a re-apply to the live `~/.claude/settings.json` strips
/// any key the new map dropped — passing the new keys instead would leak a removed
/// entry into the live file. Mirrors [`edit_profile_model`].
pub(crate) fn edit_profile_env(
    config: &mut AppConfig,
    name: &str,
    env: BTreeMap<String, String>,
) -> Result<()> {
    with_state_lock(|| {
        let profile = config.find_mut(name).context("profile not found")?;
        // Snapshot before overwrite — a removed key is only stripped from live
        // settings when it appears in `prev` but not in the new `profile.env`.
        let old_env_keys: Vec<String> = profile.env.keys().cloned().collect();
        profile.env = env;
        save_profile(profile)?;

        if config.is_active(name) {
            let profile = config.find(name).context("profile not found")?;
            apply_profile_to_claude_settings(profile, &old_env_keys)?;
        }
        Ok(())
    })
}

/// Which source a candidate custom env key collides with, in priority order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnvKeyCollision {
    /// A clauth-managed key derived from a profile field; carries the field's
    /// human label (`the base url field`, …).
    Managed(&'static str),
    /// Already a custom env entry on this account; carries the sorted index.
    ProfileField(usize),
    /// Already present in the inherited `~/.claude/settings.json` `env` block.
    BaseSettings,
}

/// Classify a candidate custom env key against the three sources, highest
/// priority first: a clauth-managed field key, then this account's existing
/// custom entries, then the inherited base `settings.json`. The managed and
/// own-field checks return before the base check, so a base hit means a key set
/// outside clauth. `base_env_keys` is read from the live settings by the caller.
pub(crate) fn classify_env_key(
    profile: &Profile,
    base_env_keys: &[String],
    candidate: &str,
) -> Option<EnvKeyCollision> {
    if let Some(label) = managed_env_key_label(candidate) {
        return Some(EnvKeyCollision::Managed(label));
    }
    if let Some(idx) = profile.env.keys().position(|k| k == candidate) {
        return Some(EnvKeyCollision::ProfileField(idx));
    }
    base_env_keys
        .iter()
        .any(|k| k == candidate)
        .then_some(EnvKeyCollision::BaseSettings)
}

pub(crate) fn rename_profile(config: &mut AppConfig, old: &str, new: &str) -> Result<()> {
    with_state_lock(|| {
        let old_dir = profile_dir(old)?;
        if old_dir.exists() {
            std::fs::rename(&old_dir, profile_dir(new)?)
                .with_context(|| format!("failed to rename profile directory to '{new}'"))?;
        }

        let was_active = config.is_active(old);
        config.rename_all_occurrences(old, new);

        save_app_state(&config.state)?;

        if was_active {
            link_profile_credentials(new)?;
        }
        Ok(())
    })?;
    // The dir move carried the durable `/profile` stamp to `new`, so only the OLD
    // name's memo is left — authoritative over a stamp no longer under that name.
    // Sequential, never inside the closure: `ProfileTtl` (450) ranks outside the
    // state flock (500), so this asserts if it ever moves in — see that rank's doc
    // for why the clock's file IO must not hold a cross-process flock.
    crate::usage::expire_profile_ttl(old);
    Ok(())
}

pub(crate) fn delete_profile(config: &mut AppConfig, name: &str, force: bool) -> Result<()> {
    with_state_lock(|| {
        // Refuse to pull an account out from under a running `clauth start`
        // session (either flavor), checked before any removal so a refused
        // delete is a clean no-op. `--yes` skips the confirm prompt but does NOT
        // override this; only `force` does.
        if !force && crate::runtime::has_live_session(name) {
            bail!("'{name}' is running a session, pass --force to delete it anyway");
        }

        let was_active = config.is_active(name);
        // An active API profile's base_url + api_key (and model-tier keys) live in
        // ~/.claude/settings.json, not the credentials link. Capture its custom
        // env keys before removal so the unwire below can strip those too.
        let active_env_keys: Vec<String> = if was_active {
            config
                .find(name)
                .map(|p| p.env.keys().cloned().collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        // Unwire the active account from the live credentials link + settings.json
        // BEFORE any irreversible local removal. These are fallible external
        // writes: running them first means a failure leaves both the record and
        // the dir intact and fully retryable, rather than stranding the api key in
        // plaintext settings.json with the profile record already gone. A blank
        // profile clears its endpoint/key/model env so the key can't linger and
        // the next session doesn't route to a dead endpoint.
        if was_active {
            clear_claude_credentials()?;
            let blank = Profile::new(name.to_string(), None, None);
            apply_profile_to_claude_settings(&blank, &active_env_keys)?;
        }

        // Dir before state: a failed removal keeps the profile in state so the
        // user can retry; persisting state first would leave an orphan dir.
        let dir = profile_dir(name)?;
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("failed to delete profile directory for '{name}'"))?;
        }
        config.remove(name);
        save_app_state(&config.state)?;
        Ok(())
    })?;
    // `remove_dir_all` took the durable stamp with it; the memo would outlive the
    // profile and mute the first `/profile` of a same-name relogin inside the hour.
    // Outside the closure — see `rename_profile` on the rank order.
    crate::usage::expire_profile_ttl(name);
    Ok(())
}

pub(crate) fn create_blank_profile(
    config: &mut AppConfig,
    name: String,
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
) -> Result<()> {
    with_state_lock(|| {
        let mut profile = Profile::new(name, base_url, api_key);
        // Part of the same single save as the profile itself — a chained
        // edit-after-create would leave a saved-but-model-less profile behind
        // when the second write fails, reported as a flat "create failed".
        profile.models.default = model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        save_profile(&profile)?;
        config.add(profile);
        save_app_state(&config.state)
    })
}

/// Set a profile's default `model` (the Setup tab's base model row / the
/// `clauth login --model` flag), preserving any alias overrides already on it.
/// An empty (post-trim) value clears the default, matching the Setup tab's ⏎
/// commit on the model row. Persists via [`edit_profile_model`], so a caller
/// that runs this before starting a session (`clauth login`) has the model
/// routed into that session's runtime settings from the first launch.
pub(crate) fn set_profile_default_model(
    config: &mut AppConfig,
    name: &str,
    raw_model: &str,
) -> Result<()> {
    let mut models = config
        .find(name)
        .map(|p| p.models.clone())
        .unwrap_or_default();
    let trimmed = raw_model.trim();
    models.default = (!trimmed.is_empty()).then(|| trimmed.to_string());
    edit_profile_model(config, name, models)
}

/// Which profile the CURRENT live login (`~/.claude/.credentials.json`)
/// belongs to, fully offline. Two tiers, tried in order:
///
/// **Token equality** (authoritative): the live refresh OR access token equals
/// a profile's stored pair — the live file IS that profile's credential. Never
/// stale, so it wins outright when it hits.
///
/// **Account uuid** (fallback, only when token equality misses): a sibling's
/// genuine re-login through Claude Code mints all-new tokens that match no
/// stored pair, so tier 1 reads UNKNOWN — and a configured `overwrite`/`new`
/// default would then capture that login into the WRONG (active) profile. This
/// tier matches CC's own identity record (`~/.claude.json`'s
/// `oauthAccount.accountUuid`) against each profile's cached anchor
/// (`profile_cache::ACCOUNT_ID_CACHE_FILE`). A missing/unparseable file, a
/// missing block, or a blank uuid on either side yields no match — two blanks
/// never prove identity.
///
/// Returns the owning profile's name — possibly the ACTIVE profile itself (a
/// same-account divergence the adopt path self-heals). Callers wanting a SIBLING
/// compare against the active name. `None` when neither tier proves ownership: a
/// genuinely foreign account, which is a human decision.
///
/// Staleness caveat: CC trusts the cached `oauthAccount` block and does not
/// re-derive it from a swapped credentials file (exactly why clauth strips it on
/// switch — [`crate::claude_json::strip_home_oauth_account`]). So a tier-2 hit is "CC's
/// last booted identity", not fresh proof of the live token's account. That can
/// only bias the verdict conservatively: pointing at a SIBLING routes the
/// divergence to the banner (user decides), and pointing at the active profile
/// is filtered out by the caller (`note_divergence` drops an owner equal to
/// active) — the same as no match, so the configured default applies unchanged.
/// The tier can never manufacture the one harmful outcome — auto-capturing a
/// sibling's login into the wrong profile — so its worst case is the banner.
pub(crate) fn identify_live_login_owner(config: &AppConfig) -> Option<String> {
    let live = read_claude_credentials().ok().flatten()?;
    let live_access = live.access_token().filter(|t| !t.is_empty());
    let live_refresh = live.refresh_token().filter(|t| !t.is_empty());

    // Tier 1 — token equality: authoritative, never stale.
    if let Some(owner) = config.profiles.iter().find(|p| {
        (live_refresh.is_some() && p.refresh_token() == live_refresh)
            || (live_access.is_some() && p.access_token() == live_access)
    }) {
        return Some(owner.name.as_str().to_string());
    }

    // Tier 2 — account uuid: a sibling's CC re-login mints fresh tokens tier 1
    // can't recognize, so match CC's cached identity against the anchor instead.
    let live_uuid = crate::claude_json::home_oauth_account_uuid()?;
    config.profiles.iter().find_map(|p| {
        let anchor = crate::profile_cache::load_profile_cache::<String>(
            &p.name,
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        )?;
        let anchor = anchor.trim();
        (!anchor.is_empty() && anchor == live_uuid.as_str()).then(|| p.name.as_str().to_string())
    })
}

/// Returns a profile whose `refresh_token` matches `live`. Matches on refresh
/// token only (stable identity); access tokens rotate and would produce false
/// misses and duplicate profiles.
pub(crate) fn find_matching_oauth_profile<'a>(
    config: &'a AppConfig,
    live: Option<&ClaudeCredentials>,
) -> Option<&'a str> {
    let live_refresh = live?.refresh_token().filter(|t| !t.is_empty())?;
    config
        .profiles
        .iter()
        .find(|p| p.refresh_token() == Some(live_refresh))
        .map(|p| p.name.as_str())
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureSnapshot {
    pub(crate) credentials: Option<ClaudeCredentials>,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    /// The account uuid an interactive login's own `/profile` probe saw these
    /// credentials authenticate as. Travels with the snapshot so whichever
    /// function COMMITS it seeds the identity anchor — including the paths that
    /// park the snapshot in a confirm modal first. `None` for a snapshot with no
    /// proven identity (a probe failure, or [`capture_snapshot`] reading live
    /// credentials off disk); that seeds nothing and leaves any existing anchor
    /// alone, exactly as before.
    pub(crate) account_uuid: Option<String>,
}

pub(crate) fn capture_snapshot() -> Result<CaptureSnapshot> {
    let credentials = read_claude_credentials()?;
    let ClaudeEndpoint { base_url, api_key } = read_claude_endpoint_config()?;
    Ok(CaptureSnapshot {
        credentials,
        base_url,
        api_key,
        // Read off disk, not from a login — this snapshot proves no identity.
        account_uuid: None,
    })
}

pub(crate) fn capture_into_profile(
    config: &mut AppConfig,
    name: String,
    snapshot: CaptureSnapshot,
) -> Result<()> {
    let CaptureSnapshot {
        credentials,
        base_url,
        api_key,
        account_uuid,
    } = snapshot;
    let seed_name = name.clone();
    with_state_lock(|| {
        let mut profile = Profile::new(name.clone(), base_url, api_key);
        profile.credentials = credentials;
        save_profile(&profile)?;
        config.add(profile);
        // AUTH-1: a fresh login/capture clears any stale auth-broken quarantine
        // for this name (e.g. a delete-then-relogin of a revoked account).
        config.set_auth_broken(&name, false);

        if config.state.active_profile.is_none() {
            link_profile_credentials(&name)?;
            config.state.active_profile = Some(name.into());
        }
        save_app_state(&config.state)
    })?;
    // Only once the credentials are committed, and only here — no caller seeds
    // its own anchor, so no caller can forget to.
    crate::usage::seed_login_anchor(&seed_name, account_uuid.as_deref());
    Ok(())
}

/// Create a fresh OAuth profile from an in-memory minted login — the Setup
/// tab's capture-then-commit path (`create account` consuming the draft-held
/// mint). One save carries credentials + model so a failed write never leaves
/// a half-configured profile behind; the first profile links + activates
/// exactly like [`capture_into_profile`].
pub(crate) fn create_profile_from_login(
    config: &mut AppConfig,
    name: String,
    model: Option<String>,
    credentials: ClaudeCredentials,
    account_uuid: Option<String>,
) -> Result<()> {
    let seed_name = name.clone();
    with_state_lock(|| {
        let mut profile = Profile::new(name.clone(), None, None);
        profile.models.default = model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        profile.credentials = Some(credentials);
        save_profile(&profile)?;
        config.add(profile);

        if config.state.active_profile.is_none() {
            link_profile_credentials(&name)?;
            config.state.active_profile = Some(name.into());
        }
        save_app_state(&config.state)
    })?;
    // The draft parked the login's uuid until `create account` fixed the name;
    // this is that name, so the anchor lands here rather than at the call site.
    crate::usage::seed_login_anchor(&seed_name, account_uuid.as_deref());
    Ok(())
}

/// Capture-name collision (issue #7): replace an EXISTING profile's credential
/// set with the freshly captured snapshot, mutating it in place. Never
/// delete+append — that would duplicate the name and desync `state.profiles`
/// and `fallback_chain`, which both index by name already, so the target
/// simply keeps its chain position, env, model settings, and `auto_start`.
/// `usage_history.jsonl` is a persisted log, not a cache, and is left alone;
/// the per-profile fetch caches (`usage_cache.json`, `third_party_cache.json`,
/// `throughput_cache.json`) describe the OLD account and are dropped so the
/// UI doesn't show stale numbers under the swapped-in credentials. The
/// `/profile` TTL clock describes the old account too and is expired for the
/// same reason — otherwise the swapped-in account's tier stays unfetched (and,
/// with `usage_cache.json` just dropped, unrendered) for up to an hour. A
/// snapshot carrying a proven identity (`account_uuid`, from an interactive
/// login's probe) re-anchors the profile here, on the commit — the confirm-gated
/// relogin parks the snapshot in a modal, so the anchor can only be seeded by
/// whoever finally commits it.
pub(crate) fn overwrite_captured_profile(
    config: &mut AppConfig,
    name: &str,
    snapshot: CaptureSnapshot,
) -> Result<()> {
    let CaptureSnapshot {
        credentials,
        base_url,
        api_key,
        account_uuid,
    } = snapshot;
    with_state_lock(|| {
        let provider = base_url.as_deref().and_then(Provider::from_base_url);
        let was_active = config.is_active(name);
        let profile = config
            .find_mut(name)
            .with_context(|| format!("profile '{name}' vanished before overwrite"))?;
        profile.base_url = base_url;
        profile.api_key = api_key;
        profile.credentials = credentials;
        profile.provider = provider;
        profile.usage = None;
        profile.fetch_status = None;
        profile.third_party_usage = None;
        save_profile(profile)?;

        for file in [
            crate::profile_cache::USAGE_CACHE_FILE,
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
            crate::throughput::THROUGHPUT_CACHE_FILE,
        ] {
            crate::profile_cache::remove_profile_cache(name, file);
        }

        if config.state.active_profile.is_none() {
            link_profile_credentials(name)?;
            config.state.active_profile = Some(name.into());
        } else if was_active {
            // The overwritten profile is (and stays) the active one: unlike a
            // brand-new capture, `save_profile` just rewrote credentials.json
            // in place (or removed it, if the snapshot had none — a third-
            // party capture). Re-run `link_profile_credentials` so the live
            // `.credentials.json` symlink is recreated against the new file,
            // or dropped instead of left dangling when the file is now gone;
            // and re-apply `base_url`/`api_key` to `settings.json` the same
            // way `edit_profile_endpoint` does, so a running `claude` doesn't
            // keep reading the OLD endpoint/token until the next switch.
            link_profile_credentials(name)?;
            let profile = config.find(name).context("profile not found")?;
            let prev_env_keys: Vec<String> = profile.env.keys().cloned().collect();
            apply_profile_to_claude_settings(profile, &prev_env_keys)?;
        }
        // AUTH-1: re-authenticating an existing profile (`clauth login <name>`) is
        // the documented recovery for a revoked login — clear its quarantine.
        // Pinned by `reauth_overwrite_clears_broken_flag`.
        config.set_auth_broken(name, false);
        save_app_state(&config.state)
    })?;
    // Outside the closure — see `rename_profile` on the rank order. Skipped when
    // the swap fails, which is imprecise rather than atomic: a failure after
    // `save_profile` leaves the new credentials on disk under the old account's
    // stamp. Bounded either way — an unexpired stamp lapses within the hour, and a
    // tick racing the gap between the flock release and this expire spends the
    // stale stamp once or loses a fresh one and re-pulls once.
    crate::usage::expire_profile_ttl(name);
    // Same commit-or-nothing rule for the identity: only credentials this profile
    // now actually holds may be vouched for by its anchor. The same
    // failure-after-`save_profile` window is NOT bounded here the way the stamp's
    // is: the anchor would keep proving the old account against the new pair, and
    // `seed_identity_anchor`'s ride-along is write-if-missing, so nothing corrects
    // it until the next successful login.
    crate::usage::seed_login_anchor(name, account_uuid.as_deref());
    Ok(())
}

/// Blank a profile's OAuth login: drop its stored credentials and per-account
/// fetch caches, returning it to the credential-less shell `Profile::new`
/// produces. Keeps name, model, env, and chain slot. When it's the active
/// profile, clear the live `~/.claude` link and deactivate — a credential-less
/// profile can't be meaningfully active, and the honest state is "no active".
pub(crate) fn clear_profile_credentials(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
        let was_active = config.is_active(name);
        let profile = config
            .find_mut(name)
            .with_context(|| format!("profile '{name}' not found"))?;
        profile.credentials = None;
        profile.usage = None;
        profile.fetch_status = None;
        profile.third_party_usage = None;
        save_profile(profile)?;
        // Drop any uncommitted rotation sidecar too: with credentials.json gone,
        // `recover_pending_credentials` would treat the sidecar as a failed commit
        // and resurrect the just-deleted login on next load.
        crate::profile::clear_staged_credentials(name);

        for file in [
            crate::profile_cache::USAGE_CACHE_FILE,
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
            crate::throughput::THROUGHPUT_CACHE_FILE,
        ] {
            crate::profile_cache::remove_profile_cache(name, file);
        }

        if was_active {
            clear_claude_credentials()?;
            config.state.active_profile = None;
            save_app_state(&config.state)?;
        }
        Ok(())
    })?;
    // The dropped login's TTL clock is the old account's; a re-login into this
    // shell must pull its own tier now, not an hour from now. Outside the closure
    // — see `rename_profile` on the rank order. Skipped when the logout fails,
    // which `clear_claude_credentials` makes imprecise rather than atomic: the
    // stored credentials are already gone by then, with the stamp left to lapse.
    crate::usage::expire_profile_ttl(name);
    Ok(())
}

/// Setup-tab "log out" for an API account: drop the stored api key while keeping
/// the base-url shell so it stays an API account you can re-login. The OAuth arm
/// is [`clear_profile_credentials`]; this one reuses [`edit_profile_endpoint`],
/// which re-derives the provider, drops stale third-party stats, and re-applies
/// the live `settings.json` (removing `ANTHROPIC_AUTH_TOKEN`) when the account is
/// active — so a running `claude` loses the token too. The account stays active:
/// its base url is still wired, only the key is gone.
pub(crate) fn clear_profile_api_key(config: &mut AppConfig, name: &str) -> Result<()> {
    with_state_lock(|| {
        let base_url = config.find(name).and_then(|p| p.base_url.clone());
        edit_profile_endpoint(config, name, base_url, None)?;
        // The endpoint editor clears the in-memory stats; also drop the on-disk
        // third-party cache so a stale copy can't resurface on reload (no key left
        // to refresh it).
        if let Some(path) = crate::profile_cache::profile_cache_path(
            name,
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        ) {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    })
}

pub(crate) fn reorder_profile(config: &mut AppConfig, from: usize, to: usize) -> Result<()> {
    if from == to || from >= config.profiles.len() || to >= config.profiles.len() {
        return Ok(());
    }
    with_state_lock(|| {
        // Resync to fix length drift from a partial save in a prior session.
        config.sync_state_profiles();
        let profile = config.profiles.remove(from);
        config.profiles.insert(to, profile);
        let name = config.state.profiles.remove(from);
        config.state.profiles.insert(to, name);
        save_app_state(&config.state)
    })
}

#[cfg(test)]
#[path = "../tests/inline/actions.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/inline/mcp_switch.rs"]
mod tests_mcp_switch;
