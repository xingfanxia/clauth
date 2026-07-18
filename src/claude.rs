use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::lock::with_state_lock;
use crate::profile::{
    AppConfig, ClaudeCredentials, Profile, atomic_write_600, claude_dir, profile_dir,
    read_json_file, save_profile,
};

fn claude_credentials_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join(".credentials.json"))
}

fn claude_settings_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join("settings.json"))
}

/// CLA-SPLIT: true when the profile carries a long-lived session token
/// (`session-token.json`, e.g. a `claude setup-token` mint). Such a profile
/// splits its credentials: the STATIC session token is what switches install
/// for Claude Code sessions to run on, while the rotating OAuth pair in
/// `credentials.json` stays clauth-private for usage polling. Sessions then
/// hold a token that never rotates, so they can never race clauth's refresher
/// on a single-use refresh chain (the root cause of the 2026-07-16..18
/// serial `refresh token revoked` deaths: N live sessions + clauth all
/// rotating the same chains through one live slot).
pub(crate) fn has_session_token(name: &str) -> bool {
    profile_dir(name)
        .map(|d| d.join("session-token.json").exists())
        .unwrap_or(false)
}

/// The file a switch INSTALLS as the live login: the profile's
/// `session-token.json` when present ([`has_session_token`]), else its
/// `credentials.json` — which is exactly the pre-split behavior, so profiles
/// without the sidecar are byte-identical to before.
pub(crate) fn install_source_path(name: &str) -> Result<PathBuf> {
    let dir = profile_dir(name)?;
    let session = dir.join("session-token.json");
    if session.exists() {
        return Ok(session);
    }
    Ok(dir.join("credentials.json"))
}

/// State of `~/.claude/.credentials.json` relative to a profile's stored credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkState {
    /// Symlink resolves to the profile's stored credentials, OR a regular file
    /// whose live OAuth access token matches the profile's stored one (macOS: Claude
    /// Code rewrites the file from the Keychain, replacing our symlink with an
    /// identical-content regular file — not divergence).
    LinkedTo,
    /// Path exists and its live credential differs from the profile's stored one —
    /// a genuine CC re-login / token rotation the user may want to capture.
    Diverged,
    /// Path does not exist.
    Missing,
}

pub(crate) fn classify_credentials_link(active: &str) -> Result<LinkState> {
    let link = claude_credentials_path()?;
    // CLA-SPLIT: the live slot is compared against what a switch INSTALLS —
    // for a session-token profile that's the static token, so a live slot
    // holding it classifies LinkedTo and the whole divergence machinery
    // stays dormant (a static token never rotates out from under us).
    let expected = install_source_path(active)?;
    classify_link_at(&link, &expected)
}

/// Classify a symlink at `link` against `expected`; canonical paths when resolvable.
pub(crate) fn classify_link_at(link: &Path, expected: &Path) -> Result<LinkState> {
    let meta = match link.symlink_metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LinkState::Missing),
        Err(e) => return Err(e).context("failed to stat .credentials.json"),
    };
    if !meta.file_type().is_symlink() {
        // The live credentials are a regular file, not our symlink. This is
        // handled the same way on EVERY platform (not gated to macOS): trust
        // content, not symlink identity, so an ordinary switch doesn't falsely
        // prompt to capture credentials that already match the profile. macOS is
        // where it happens on every run — Claude Code rewrites
        // ~/.claude/.credentials.json as a regular-file mirror of the Keychain,
        // clobbering our symlink — but the same regular-file state can arise on
        // Linux/Windows if anything replaces the symlink, and the correct answer
        // is identical: equal access token ⇒ LinkedTo, otherwise a genuine
        // re-login / rotation ⇒ Diverged. (Gating this to macOS would make a
        // non-symlink file on other platforms fall through to `read_link` below
        // and error.)
        return Ok(
            match (
                read_json_file::<ClaudeCredentials>(link),
                read_json_file::<ClaudeCredentials>(expected),
            ) {
                (Ok(live), Ok(stored))
                    if live.access_token().is_some_and(|t| !t.is_empty())
                        && live.access_token() == stored.access_token() =>
                {
                    LinkState::LinkedTo
                }
                _ => LinkState::Diverged,
            },
        );
    }
    let target = std::fs::read_link(link).context("failed to read .credentials.json link")?;
    if paths_equivalent(&target, expected) {
        Ok(LinkState::LinkedTo)
    } else {
        Ok(LinkState::Diverged)
    }
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// True when the profile has no stored credentials but the live path is a regular
/// file with a completed OAuth login — first login after blank profile creation.
/// clauth adopts this rather than treating it as divergence.
/// SipHash of the live access token — a cheap identity for "which login sits
/// in `~/.claude/.credentials.json` right now". It changes on every re-login
/// and every refresh, so memos keyed to it release exactly when the creds
/// change. `None` when no readable OAuth login is present. Shared by the
/// TUI's divergence banner and the daemon's follow/refusal memos.
pub(crate) fn live_credentials_fingerprint() -> Option<u64> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let creds = read_claude_credentials().ok().flatten()?;
    let token = creds.access_token().filter(|t| !t.is_empty())?;
    let mut hasher = DefaultHasher::new();
    token.hash(&mut hasher);
    Some(hasher.finish())
}

/// True when the live credentials hold NO usable login: no OAuth block, or one
/// whose access AND refresh tokens are both absent/blank — Claude Code's
/// logged-out shell (it blanks the tokens and zeroes `expiresAt` when its own
/// refresh dies, keeping unrelated keys like `mcpOAuth`). A shell still
/// classifies [`LinkState::Diverged`], but there is no login in it to protect.
pub(crate) fn live_login_is_empty(creds: &ClaudeCredentials) -> bool {
    creds.access_token().filter(|t| !t.is_empty()).is_none()
        && creds.refresh_token().filter(|t| !t.is_empty()).is_none()
}

pub(crate) fn is_first_login(active: &str) -> Result<bool> {
    let link = claude_credentials_path()?;
    // CLA-SPLIT: a profile whose install source is its session token is never
    // "credential-less" — a live OAuth login must not be adopted over it.
    let expected = install_source_path(active)?;
    Ok(is_first_login_at(&link, &expected))
}

/// Path-based core of [`is_first_login`], split for testing. The OAuth check
/// rejects partial writes (e.g. `{}`) so adoption waits for a completed login.
fn is_first_login_at(link: &Path, expected: &Path) -> bool {
    if expected.exists() {
        return false;
    }
    let Ok(meta) = link.symlink_metadata() else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return false;
    }
    std::fs::read(link)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ClaudeCredentials>(&bytes).ok())
        .is_some_and(|creds| creds.claude_ai_oauth.is_some())
}

pub(crate) fn read_claude_credentials() -> Result<Option<ClaudeCredentials>> {
    let path = claude_credentials_path()?;
    if !path.exists() {
        return Ok(None);
    }
    read_json_file(&path).map(Some)
}

/// Outcome of [`write_live_oauth_pair`]: written, or benignly superseded by a
/// concurrent actor (nothing lost, nothing to recover).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveWriteBack {
    Written,
    /// The live slot changed hands between the caller's judgment and this
    /// write — a fresh CC login landed, or a profile's own store took the
    /// slot (symlink). The rotated pair continued a lineage that fresh login
    /// just superseded; discarding it loses nothing.
    Superseded,
}

/// Surgically update the LIVE `.credentials.json`'s `claudeAiOauth` block with
/// a freshly rotated pair, preserving every other top-level key (Claude Code
/// also parks e.g. `mcpOAuth` entries in this file — a struct round-trip would
/// silently drop them). Used by the daemon's dead-login rescue (RESCUE-1) when
/// its confirm-dead refresh probe SUCCEEDED: the probe consumed the file's
/// single-use refresh token, so the fresh pair MUST land back in the file or
/// the live session's chain is destroyed.
///
/// RESCUE-2c: the authoritative guards live INSIDE the state flock,
/// immediately before the mutation — a symlinked live path (a profile's own
/// store took the slot; writing an unowned pair through it would corrupt that
/// profile) and a fingerprint that no longer matches `expected_fingerprint`
/// (a fresh CC login landed) both return [`LiveWriteBack::Superseded`]
/// instead of clobbering.
pub(crate) fn write_live_oauth_pair(
    tokens: &crate::oauth::TokenResponse,
    expected_fingerprint: Option<u64>,
) -> Result<LiveWriteBack> {
    with_state_lock(|| {
        let path = claude_credentials_path()?;
        let meta = path
            .symlink_metadata()
            .context("live .credentials.json vanished mid-rescue")?;
        if meta.file_type().is_symlink() || live_credentials_fingerprint() != expected_fingerprint {
            return Ok(LiveWriteBack::Superseded);
        }
        let bytes = std::fs::read(&path).context("failed to read live .credentials.json")?;
        let mut root: serde_json::Value =
            serde_json::from_slice(&bytes).context("live .credentials.json is not JSON")?;
        let obj = root
            .as_object_mut()
            .context("live .credentials.json is not a JSON object")?;
        let oauth = obj
            .entry("claudeAiOauth")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let oauth = oauth
            .as_object_mut()
            .context("live claudeAiOauth is not a JSON object")?;
        oauth.insert("accessToken".into(), tokens.access_token.clone().into());
        oauth.insert("refreshToken".into(), tokens.refresh_token.clone().into());
        // CC stores epoch-ms; `expires_in` is seconds-from-now.
        let expires_at = crate::usage::now_ms().saturating_add(tokens.expires_in * 1000);
        oauth.insert("expiresAt".into(), expires_at.into());
        if let Some(scope) = tokens.scope.as_deref().filter(|s| !s.trim().is_empty()) {
            let scopes: Vec<serde_json::Value> =
                scope.split_whitespace().map(|s| s.into()).collect();
            oauth.insert("scopes".into(), scopes.into());
        }
        let out = serde_json::to_vec(&root).context("failed to serialize live credentials")?;
        atomic_write_600(&path, out).context("failed to write live .credentials.json")?;
        Ok(LiveWriteBack::Written)
    })
}

/// [`force_link_profile_credentials`] with the caller's judgment re-verified
/// INSIDE the state flock, immediately before the mutation (RESCUE-2c). The
/// reclaim's `still_unchanged` re-check used to run outside the lock, leaving
/// a syscall-wide window in which a concurrently landed CC login could be
/// destroyed by the unconditional relink. Returns `false` (no-op) when the
/// re-check fails — the caller treats that as "superseded, start over".
pub(crate) fn force_link_profile_credentials_if(
    name: &str,
    still_unchanged: &dyn Fn() -> bool,
) -> Result<bool> {
    with_state_lock(|| {
        if !still_unchanged() {
            return Ok(false);
        }
        force_link_profile_credentials(name).map(|()| true)
    })
}

/// True when the live `.credentials.json` currently parses to such a logged-out
/// shell. An unreadable or non-JSON file reads `false` — it may be a Claude
/// Code write in progress, and "possibly a login" keeps the same protection as
/// a real one (the divergence guards stay armed).
pub(crate) fn live_credentials_are_shell() -> bool {
    matches!(
        read_claude_credentials(),
        Ok(Some(live)) if live_login_is_empty(&live)
    )
}

/// macOS: mirror a profile's stored OAuth login into the Keychain so Claude Code
/// (which reads the Keychain, not the file) actually switches account. No-op when
/// the profile has no stored `credentials.json` (a base_url profile, whose
/// endpoint+token come from `settings.json`, or an OAuth profile not yet logged
/// in) — the existing Keychain login is left untouched in that case.
///
/// Runs after the symlink swap and is `?`-fatal: a failure leaves the file layer
/// switched while CC still reads the old Keychain login. Loud + recoverable —
/// both writes are idempotent, so retrying the switch re-runs the pair.
#[cfg(target_os = "macos")]
fn keychain_write_source(path: &Path) -> Result<()> {
    // CLA-SPLIT: callers pass the already-resolved install source so the
    // symlink target and the Keychain content come from ONE resolution — a
    // session-token.json vanishing between two stats can't split them.
    if !path.exists() {
        return Ok(());
    }
    let creds: ClaudeCredentials = read_json_file(path)?;
    crate::keychain::keychain_write(&creds)
}

#[cfg(unix)]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).context("failed to create credential symlink")
}

#[cfg(windows)]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    match std::os::windows::fs::symlink_file(target, link) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::copy(target, link)
            .map(|_| ())
            .context("failed to copy credentials"),
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    std::fs::copy(target, link)
        .map(|_| ())
        .context("failed to copy credentials")
}

/// Symlink `~/.claude/.credentials.json` → profile's `credentials.json` (copy on
/// Windows). Refuses to overwrite a non-matching regular file — that would silently
/// drop a CC re-login the user hasn't resolved yet.
pub(crate) fn link_profile_credentials(name: &str) -> Result<()> {
    with_state_lock(|| {
        let link = claude_credentials_path()?;
        let target = install_source_path(name)?;

        if let Ok(meta) = link.symlink_metadata() {
            if !meta.file_type().is_symlink() {
                let live_bytes = std::fs::read(&link).ok();
                let target_bytes = std::fs::read(&target).ok();
                if live_bytes != target_bytes {
                    anyhow::bail!(
                        "refusing to replace .credentials.json: live file differs from profile '{name}'; {} first",
                        crate::format::RESOLVE_IN_TUI
                    );
                }
            }
            std::fs::remove_file(&link).context("failed to remove old .credentials.json")?;
        }

        if target.exists() {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            create_symlink(&target, &link)?;
            // macOS: make the switch real — Claude Code reads the Keychain.
            #[cfg(target_os = "macos")]
            if crate::keychain::enabled() {
                keychain_write_source(&target)?;
            }
        }

        Ok(())
    })
}

pub(crate) fn clear_claude_credentials() -> Result<()> {
    with_state_lock(|| {
        let link = claude_credentials_path()?;
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("failed to remove .credentials.json")?;
        }
        // macOS: also drop the live Keychain login so Claude Code can't spend the
        // account (parity with removing the credential file). This deletes
        // whatever the item holds at that moment — possibly a chain CC rotated
        // after our last capture, or a login clauth never wrote. The write-only
        // design can't snapshot it first (reading Claude's item prompts on every
        // call), so that tail is lost and needs a re-login; see keychain.rs.
        #[cfg(target_os = "macos")]
        if crate::keychain::enabled() {
            crate::keychain::keychain_delete()?;
        }
        Ok(())
    })
}

pub(crate) struct ClaudeEndpoint {
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
}

pub(crate) fn read_claude_endpoint_config() -> Result<ClaudeEndpoint> {
    let path = claude_settings_path()?;
    if !path.exists() {
        return Ok(ClaudeEndpoint {
            base_url: None,
            api_key: None,
        });
    }
    let settings: serde_json::Value = read_json_file(&path)?;
    Ok(ClaudeEndpoint {
        base_url: settings["env"]["ANTHROPIC_BASE_URL"]
            .as_str()
            .map(str::to_owned),
        api_key: settings["env"]["ANTHROPIC_AUTH_TOKEN"]
            .as_str()
            .map(str::to_owned),
    })
}

/// The Setup-tab field that owns a clauth-managed env key, phrased for the
/// collision prompt (`'X' is already set by …`). These are the keys clauth
/// derives from a profile's endpoint + model-tier fields; a custom env entry
/// equal to one of them would override the field's value in `settings.json`.
/// `None` when the key is not clauth-managed.
pub(crate) fn managed_env_key_label(key: &str) -> Option<&'static str> {
    Some(match key {
        "ANTHROPIC_BASE_URL" => "the base url field",
        "ANTHROPIC_AUTH_TOKEN" => "the api key field",
        "ANTHROPIC_DEFAULT_OPUS_MODEL" => "the opus model field",
        "ANTHROPIC_DEFAULT_SONNET_MODEL" => "the sonnet model field",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL" => "the haiku model field",
        "CLAUDE_CODE_SUBAGENT_MODEL" => "the subagent model field",
        _ => return None,
    })
}

/// Keys present in the live `~/.claude/settings.json` `env` object. Empty when
/// the file is absent or carries no `env` block. Used to detect a custom env key
/// that already exists in the inherited base settings.
pub(crate) fn claude_settings_env_keys() -> Result<Vec<String>> {
    let path = claude_settings_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let settings: serde_json::Value = read_json_file(&path)?;
    Ok(settings["env"]
        .as_object()
        .map(|env| env.keys().cloned().collect())
        .unwrap_or_default())
}

/// Patch `settings.json` `env` with profile's endpoint keys and env map;
/// strip `prev_env_keys` the new profile doesn't carry to clear stale entries.
pub(crate) fn apply_profile_to_claude_settings(
    profile: &Profile,
    prev_env_keys: &[String],
) -> Result<()> {
    with_state_lock(|| apply_profile_to_claude_settings_inner(profile, prev_env_keys))
}

fn apply_profile_to_claude_settings_inner(
    profile: &Profile,
    prev_env_keys: &[String],
) -> Result<()> {
    let path = claude_settings_path()?;

    let has_anything = profile.base_url.is_some()
        || profile.api_key.is_some()
        || !profile.env.is_empty()
        || !profile.models.is_empty()
        || !prev_env_keys.is_empty();
    if !has_anything && !path.exists() {
        return Ok(());
    }

    let content = build_claude_settings_json(Some(&path), profile, prev_env_keys)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // TECH-9 #16: settings.json can embed a third-party api_key (as
    // ANTHROPIC_AUTH_TOKEN) — 0o600, never the umask-default 0o644 this wrote before
    // in a world-traversable ~/.claude.
    atomic_write_600(&path, content).context("failed to write settings.json")
}

/// Build the merged settings.json content. `prev_env_keys` are stripped before
/// the new profile's env is applied; pass `&[]` on start to leave existing keys.
/// Also writes the profile's model config — the top-level `model` setting and
/// the `ANTHROPIC_DEFAULT_*_MODEL` / `CLAUDE_CODE_SUBAGENT_MODEL` env keys —
/// each set when present and removed when unset, so a switch never inherits the
/// previous profile's model routing.
///
/// `base` is the settings file to merge onto; `None` (or a missing path) starts
/// from an empty object — used for an isolated runtime that must carry no
/// operator settings.
pub(crate) fn build_claude_settings_json(
    base: Option<&Path>,
    profile: &Profile,
    prev_env_keys: &[String],
) -> Result<String> {
    let mut settings: serde_json::Value = match base {
        Some(p) if p.exists() => read_json_file(p)?,
        _ => serde_json::json!({}),
    };

    if settings.get("env").is_none() {
        settings["env"] = serde_json::json!({});
    }

    let env = settings["env"]
        .as_object_mut()
        .context("settings.json `env` is not an object")?;

    for key in prev_env_keys {
        if !profile.env.contains_key(key) {
            env.remove(key);
        }
    }

    match profile.base_url.as_deref() {
        Some(url) => {
            env.insert("ANTHROPIC_BASE_URL".into(), url.into());
        }
        None => {
            env.remove("ANTHROPIC_BASE_URL");
        }
    }
    match profile.api_key.as_deref() {
        Some(key) => {
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), key.into());
        }
        None => {
            env.remove("ANTHROPIC_AUTH_TOKEN");
        }
    }

    // Model-tier and subagent overrides — clauth-owned env keys, always set or
    // cleared deterministically so a switch never inherits the prior profile's.
    let model_env = [
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", &profile.models.opus),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", &profile.models.sonnet),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", &profile.models.haiku),
        ("CLAUDE_CODE_SUBAGENT_MODEL", &profile.models.subagent),
    ];
    for (key, value) in model_env {
        match value {
            Some(v) => {
                env.insert(key.into(), v.clone().into());
            }
            None => {
                env.remove(key);
            }
        }
    }

    // Profile env last: explicit ANTHROPIC_* entries win over base_url/api_key.
    for (k, v) in &profile.env {
        env.insert(k.clone(), v.clone().into());
    }

    // Top-level `model` setting (not env). The `env` borrow above has ended, so
    // `settings` is free to mutate again.
    let obj = settings
        .as_object_mut()
        .context("settings.json is not an object")?;
    match profile.models.default.as_deref() {
        Some(model) => {
            obj.insert("model".into(), model.into());
        }
        None => {
            obj.remove("model");
        }
    }

    serde_json::to_string_pretty(&settings).context("failed to serialize settings.json")
}

/// Save live `.credentials.json` into the active profile. No-op on divergence
/// (would silently overwrite stored identity); divergence is resolved via
/// `force_snapshot_active_credentials` after user confirmation. First-login
/// on a credential-less profile is adopted instead.
///
/// CAP-1: the decision and the write share ONE read of the live file. The old
/// shape classified the link, then re-read the file to capture — a foreign
/// login landing between the two (a running `claude`'s own refresh writes back
/// whatever chain it holds, Keychain first, mirror file next) was captured
/// into a profile it does not belong to (2026-07-12: 'ax-backup' held
/// 'ax-main''s chain this way and the pair double-polled one account into a
/// rate-limit pin). Deciding on exactly the bytes that get written closes the
/// window: an equal-token live refreshes the store with those same bytes, a
/// differing one is the divergence case (never captured unattended —
/// same-account rotations are the identity-guarded adopt leg's job), and only
/// a completed first login adopts the bytes it examined.
pub(crate) fn snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|| {
        let Some(active) = config.state.active_profile.clone() else {
            return Ok(());
        };
        let live = match read_claude_credentials() {
            Ok(live) => live,
            // Unreadable/partial live file — never capture what we can't parse.
            Err(_) => return Ok(()),
        };
        let Some(live) = live else {
            // No live login at all: the snapshot records "nothing is live",
            // matching the pre-CAP-1 Missing behavior.
            return save_live_credentials(config, &active, None);
        };
        let stored_path = install_source_path(&active)?;
        if !stored_path.exists() {
            // First login on a credential-less profile: adopt only a COMPLETED
            // login (a partial write adopts nothing), using these bytes.
            if live.claude_ai_oauth.is_some() {
                return adopt_first_login_bytes(config, &active, live);
            }
            return Ok(());
        }
        let same_token = read_json_file::<ClaudeCredentials>(&stored_path)
            .ok()
            .is_some_and(|stored| {
                stored.access_token().is_some_and(|t| !t.is_empty())
                    && stored.access_token() == live.access_token()
            });
        if same_token {
            // CLA-SPLIT: a live slot holding the profile's static session
            // token carries nothing to snapshot — and writing it into
            // `profile.credentials` would clobber the clauth-private usage
            // OAuth pair. Leave both stores untouched.
            if has_session_token(&active) {
                return Ok(());
            }
            // Same access token ⇒ same rotation state: refresh the store with
            // the bytes just read (CC rewrites the mirror with identical
            // tokens but sometimes fresher metadata).
            return save_live_credentials(config, &active, Some(live));
        }
        // Diverged (or an unparseable store): keep the stored identity.
        Ok(())
    })
}

/// Store the live `.credentials.json` into the profile then replace it with a
/// symlink. Must only be called after `is_first_login` returns true; the gate
/// is re-verified here on the same read that gets written (CAP-1), so a live
/// file that changed since the caller's check adopts nothing wrong.
pub(crate) fn adopt_first_login(config: &mut AppConfig, active: &str) -> Result<()> {
    with_state_lock(|| {
        let Ok(Some(live)) = read_claude_credentials() else {
            return Ok(());
        };
        if live.claude_ai_oauth.is_none() || install_source_path(active)?.exists() {
            return Ok(()); // partial write, or no longer a first login
        }
        adopt_first_login_bytes(config, active, live)
    })
}

/// Adopt an already-read-and-verified first login: save exactly those bytes,
/// relink, and anchor the profile's identity to the login just captured.
fn adopt_first_login_bytes(
    config: &mut AppConfig,
    active: &str,
    live: ClaudeCredentials,
) -> Result<()> {
    save_live_credentials(config, active, Some(live))?;
    force_link_profile_credentials(active)?;
    // CAP-1: a capture is an identity event — the anchor moves with the store.
    crate::profile_cache::refresh_account_anchor(active);
    Ok(())
}

/// Write pre-read live credentials into `active`'s store (`None` clears it).
/// Callers decide WHAT to write; this never re-reads the live file, so the
/// bytes written are the bytes the caller's checks saw.
fn save_live_credentials(
    config: &mut AppConfig,
    active: &str,
    credentials: Option<ClaudeCredentials>,
) -> Result<()> {
    if let Some(profile) = config.find_mut(active) {
        profile.credentials = credentials;
        save_profile(profile)?;
    }
    Ok(())
}

/// Snapshot the live `.credentials.json` into the active profile unconditionally.
/// The caller has confirmed the capture (the CLI `[Y/n]`, the TUI divergence
/// Overwrite action, or an MCP Overwrite default) — so this may change which
/// ACCOUNT the profile holds. CAP-1: the identity anchor moves with the store
/// (or is dropped when no local identity hint exists), so the identity-guarded
/// adopt/follow paths never consult an anchor describing the pre-capture
/// account.
pub(crate) fn force_snapshot_active_credentials(config: &mut AppConfig) -> Result<()> {
    with_state_lock(|| {
        let Some(active) = config.state.active_profile.clone() else {
            return Ok(());
        };
        let live = read_claude_credentials()?;
        let captured_login = live.is_some();
        save_live_credentials(config, &active, live)?;
        if captured_login {
            crate::profile_cache::refresh_account_anchor(&active);
        } else {
            crate::profile_cache::drop_account_anchor(&active);
        }
        Ok(())
    })
}

/// Re-link `.credentials.json` to `name`'s stored credentials, overwriting the live path.
pub(crate) fn force_link_profile_credentials(name: &str) -> Result<()> {
    with_state_lock(|| {
        let link = claude_credentials_path()?;
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).context("failed to remove .credentials.json")?;
        }
        let target = install_source_path(name)?;
        if target.exists() {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            create_symlink(&target, &link)?;
            // macOS: make the switch real — Claude Code reads the Keychain.
            #[cfg(target_os = "macos")]
            if crate::keychain::enabled() {
                keychain_write_source(&target)?;
            }
        }
        Ok(())
    })
}

/// RESCUE-2: preserve a diverged live login before a user-ordered switch
/// discards it. Copies the regular-file live credentials into
/// `~/.clauth/quarantine/<epoch-ms>-<seq>-<active>.credentials.json` (0600)
/// so the forced switch is loss-free — a login clauth doesn't own stays
/// recoverable by hand instead of being destroyed. The newest 20 archives are
/// kept; older ones are pruned. Refuses a symlinked live path: a symlink is a
/// profile's own store, so there is nothing unsaved to archive (the
/// divergence resolved between the caller's check and this call).
pub(crate) fn archive_live_credentials(active: &str) -> Result<PathBuf> {
    with_state_lock(|| {
        let path = claude_credentials_path()?;
        let meta = path
            .symlink_metadata()
            .context("live .credentials.json vanished before archive")?;
        anyhow::ensure!(
            !meta.file_type().is_symlink(),
            "live .credentials.json is a profile's symlink — nothing unsaved to archive"
        );
        let bytes = std::fs::read(&path).context("failed to read live .credentials.json")?;
        let dir = crate::profile::clauth_dir()?.join("quarantine");
        std::fs::create_dir_all(&dir).context("failed to create quarantine dir")?;
        // The per-process sequence breaks same-millisecond collisions (two
        // archives in one ms would otherwise silently overwrite each other —
        // the exact loss the archive exists to prevent). Zero-padded so the
        // filename stays chronologically sortable past the epoch-ms prefix.
        static ARCHIVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = ARCHIVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dest = dir.join(format!(
            "{}-{seq:04}-{active}.credentials.json",
            crate::usage::now_ms()
        ));
        atomic_write_600(&dest, bytes).context("failed to write quarantine copy")?;
        // Retention: an archive, not a landfill — keep the newest
        // QUARANTINE_KEEP copies (names sort chronologically), best-effort
        // prune the rest. Pruning failure never fails the switch.
        const QUARANTINE_KEEP: usize = 20;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut archived: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(".credentials.json"))
                })
                .collect();
            archived.sort();
            if archived.len() > QUARANTINE_KEEP {
                for old in &archived[..archived.len() - QUARANTINE_KEEP] {
                    let _ = std::fs::remove_file(old);
                }
            }
        }
        Ok(dest)
    })
}

/// True when both sides have an OAuth block and access or refresh token differs.
/// Missing data on either side returns false (snapshot/skip is safer than guessing).
pub(crate) fn credentials_diverged(
    stored: Option<&ClaudeCredentials>,
    live: Option<&ClaudeCredentials>,
) -> bool {
    let Some(stored) = stored.and_then(|c| c.claude_ai_oauth.as_ref()) else {
        return false;
    };
    let Some(live) = live.and_then(|c| c.claude_ai_oauth.as_ref()) else {
        return false;
    };
    stored.access_token != live.access_token || stored.refresh_token != live.refresh_token
}

/// Replace the symlink at `.credentials.json` with a regular file (same bytes).
/// No-op if already a regular file or absent. Prevents CC writes from bleeding
/// into the profile's storage after the user disowns the active profile.
pub(crate) fn detach_credentials_link() -> Result<()> {
    with_state_lock(|| {
        let path = claude_credentials_path()?;
        let Ok(meta) = path.symlink_metadata() else {
            return Ok(());
        };
        if !meta.file_type().is_symlink() {
            return Ok(());
        }
        let content =
            std::fs::read(&path).context("failed to read .credentials.json before detach")?;
        std::fs::remove_file(&path).context("failed to remove .credentials.json symlink")?;
        atomic_write_600(&path, content).context("failed to write detached .credentials.json")?;
        Ok(())
    })
}

#[cfg(test)]
#[path = "../tests/inline/claude.rs"]
mod tests;
