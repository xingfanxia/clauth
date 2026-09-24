#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Characterization of the daemon's per-tick work (`Daemon::tick` and the
//! drains extracted to `src/daemon/tick.rs`) — the top reliability path.
//! TECH-5 — characterization of the daemon's per-tick work (`Daemon::tick` and
//! the drains extracted to `src/daemon/tick.rs`). These PIN CURRENT behavior on
//! the top reliability path so the TECH-6 queue rewrite and TECH-7 RMW fix are
//! TDD-able rather than eyeballed (the ledger already records a `drain_config_ops`
//! mtime regression on this branch caught only by manual review).
//!
//! All disk state is redirected into a [`HomeSandbox`] tempdir, and
//! `keychain::enabled()` is false under `cfg(test)`, so the switch paths exercise
//! the file/symlink model only and NEVER touch the operator's real `~/.clauth`,
//! `~/.claude`, or the `Claude Code-credentials` Keychain item (Incident C
//! guardrail). No network: every OAuth token is minted with a future expiry so the
//! pre-install auth gate returns `Ready` without a refresh.

use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use crate::profile::{
    AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, claude_dir, clauth_dir,
    load_config, save_app_state, save_profile,
};
use crate::testutil::{HomeSandbox, blank_profile, set_mtime, through_handle};
use crate::usage::{Origin, PendingSwitchEntry, enqueue_pending_switch, now_ms};

use super::{ConfigOp, Daemon};

/// Push a switch request directly onto the daemon's queue (bypassing the enqueue
/// helper) so a test can stage an exact `{origin, target}` set — including a
/// {User, Scheduler} pair that would never coexist through the helper.
fn stage_switch(d: &Daemon, target: &str, origin: Origin, retry_until: u64) {
    d.pending_switch
        .lock()
        .expect("pending_switch")
        .push_back(PendingSwitchEntry {
            target: target.into(),
            origin,
            harness: crate::profile::Harness::Claude,
            retry_until,
        });
}

/// Snapshot the queued switch targets in order, for asserting re-queue / clearing.
fn queued_targets(d: &Daemon) -> Vec<String> {
    d.pending_switch
        .lock()
        .expect("pending_switch")
        .iter()
        .map(|e| e.target.to_string())
        .collect()
}

/// Epoch-ms an hour ahead — a token with real life left, so the auth gate takes
/// the no-refresh `Ready` path.
fn future_expiry() -> i64 {
    crate::usage::now_ms() as i64 + 3_600_000
}

/// Minimal OAuth credentials whose access token round-trips through the profile
/// store; `access` also seeds a distinct refresh token so profiles never collide.
fn oauth_creds(access: &str) -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(format!("rt-{access}")),
            expires_at: Some(future_expiry()),
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    }
}

/// A blank profile with a live-token credential block attached.
fn profile_with_creds(name: &str, access: &str) -> Profile {
    let mut p = blank_profile(&crate::profile::ProfileName::from(name));
    p.credentials = Some(oauth_creds(access));
    p
}

/// Persist `profiles` + an `AppState` (given active + refresh interval) to the
/// sandbox disk, then return the matching in-memory `AppConfig` the daemon owns.
fn persist(profiles: Vec<Profile>, active: Option<&str>, refresh_interval_ms: u64) -> AppConfig {
    let mut state = AppState {
        active_profile: active.map(Into::into),
        profiles: profiles.iter().map(|p| p.name.clone()).collect(),
        refresh_interval_ms,
        ..AppState::default()
    };
    // fallback_chain left empty — the drains under test don't consult it.
    state.fallback_chain.clear();
    for p in &profiles {
        save_profile(p).expect("persist profile");
    }
    save_app_state(&state).expect("persist app state");
    AppConfig { state, profiles }
}

/// Build a daemon over `config`, writing `status.json` beside the sandbox root.
fn daemon_for(config: AppConfig) -> Daemon {
    let status_path = clauth_dir().expect("clauth dir").join("status.json");
    Daemon::new(config, status_path)
}

/// Symlink `~/.claude/.credentials.json` at the profile's stored credentials so
/// the active link classifies as `LinkedTo` (clean — no unsaved divergence).
fn link_active_clean(name: &str) {
    crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from(name))
        .expect("link active credentials");
}

/// Write `~/.claude/.credentials.json` as a REGULAR file with an access token
/// that differs from `name`'s stored one — a genuine CC re-login the daemon must
/// treat as unsaved divergence (`active_diverged_unsaved` → true).
fn diverge_active(diff_access: &str) {
    let dir = claude_dir().expect("claude dir");
    std::fs::create_dir_all(&dir).expect("mkdir ~/.claude");
    let live = dir.join(".credentials.json");
    let bytes = serde_json::to_vec(&oauth_creds(diff_access)).expect("serialize live");
    std::fs::write(&live, bytes).expect("write live credentials");
}

fn active_of(d: &Daemon) -> Option<String> {
    d.config
        .lock()
        .expect("config")
        .state
        .active_profile
        .as_deref()
        .map(str::to_string)
}

// ── tick(): the extracted loop body ───────────────────────────────────────────

/// `tick` on an idle daemon with empty queues writes `status.json` and changes
/// nothing else — the pure no-op characterization of one loop iteration. Stays
/// cross-platform: the armed throttle is what keeps the tick's heal inert here,
/// since the gate's pointer read cannot be sandboxed on Windows.
#[test]
fn tick_with_empty_queues_writes_status_and_leaves_active_unchanged() {
    let _home = HomeSandbox::new();
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);
    let status_path = daemon.status_path.clone();

    daemon.tick();

    assert!(
        status_path.exists(),
        "tick must (re)write status.json each iteration"
    );
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "no queued switch → active profile is unchanged by a tick"
    );
}

/// The tick is a heal call site: one tick over a broken registration reaches
/// `claude` through the detached heal. Deleting the call from `tick` reds here,
/// which a healthy-registration variant could never do — that one is green
/// whether or not the tick calls anything. Unix-only: the fake `claude` is a
/// shell shim.
#[cfg(unix)]
#[test]
fn tick_heals_a_broken_plugin_registration() {
    use crate::testutil::{FakeClaude, join_background_tasks, seed_broken_plugin_registration};

    let home = HomeSandbox::new();
    let fake = FakeClaude::new(&home);
    crate::plugin_host::reset_heal_throttle_for_test();
    // The tick drives the herdr heal too; its throttle is armed so this test
    // stays spawn-free beside the claude heal it pins (the herdr heal's
    // fail-closed test assert needs the injected path, which no sandbox pins).
    crate::herdr::arm_heal_throttle_for_test();
    seed_broken_plugin_registration();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    daemon.tick();
    join_background_tasks();

    assert!(
        !fake.log().is_empty(),
        "one tick over a broken registration must reach the heal"
    );
}

// ── tick vs a wedged flock holder ─────────────────────────────────────────────

/// A tick draining both queues against a wedged flock holder completes within
/// the watchdog deadline: the first drain's wait spends the tick's shared
/// window, and the second drain is SKIPPED rather than handed a fresh wait —
/// pre-fix, two full waits (2 × 25 s) aborted the daemon mid-switch past the
/// 30 s watchdog. The switch is re-queued and the switch-off stays pending, so
/// the next tick retries both with a fresh window. Short seams pose the wedge:
/// the budget override shrinks the tick's window to 300 ms and the lock-timeout
/// override keeps a broken full wait observable in ~1 s instead of the real
/// 25 s.
#[test]
fn tick_skips_the_second_drain_once_a_wedged_flock_spends_the_budget() {
    let _home = HomeSandbox::new();
    crate::lock::set_subprocess_budget_override(Some(Duration::from_millis(300)));
    crate::lock::set_state_lock_timeout_override(Some(Duration::from_secs(1)));
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();

    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    // A second open file description holding the state flock — conflicts with
    // the daemon's acquisition exactly as a wedged peer would.
    let dir = clauth_dir().expect("clauth dir");
    let holder = crate::profile::open_state_file(&dir.join(crate::lock::LOCK_FILENAME))
        .expect("open holder handle");
    holder.lock().expect("hold the flock");

    let mut daemon = daemon_for(config);
    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    *daemon
        .pending_switch_off
        .lock()
        .expect("pending_switch_off") = true;

    let start = std::time::Instant::now();
    daemon.tick();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "the tick must complete despite the wedge (within the seam-posed deadline), took {elapsed:?}"
    );
    assert_eq!(
        queued_targets(&daemon),
        vec!["beta".to_string()],
        "the wedged switch is re-queued for the next tick"
    );
    assert!(
        *daemon
            .pending_switch_off
            .lock()
            .expect("pending_switch_off"),
        "the skipped switch-off stays queued for the next tick (the pre-fix drain consumed \
         the flag before timing out)"
    );
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "no switch landed"
    );

    crate::lock::set_subprocess_budget_override(None);
    crate::lock::set_state_lock_timeout_override(None);
    drop(holder);
}

/// The healthy twin: with the flock free, a tick draining both queues runs
/// BOTH drains exactly as before the bound — the switch lands, then the
/// switch-off lands, and nothing is skipped or re-ordered. This pins the
/// byte-identical healthy path the aggregate bound must not disturb.
#[test]
fn tick_drains_both_queues_when_the_flock_is_free() {
    let _home = HomeSandbox::new();
    crate::plugin_host::arm_heal_throttle_for_test();
    crate::herdr::arm_heal_throttle_for_test();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);
    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    *daemon
        .pending_switch_off
        .lock()
        .expect("pending_switch_off") = true;

    daemon.tick();

    assert_eq!(
        active_of(&daemon).as_deref(),
        None,
        "the switch AND the switch-off both landed"
    );
    assert_eq!(
        queued_targets(&daemon),
        Vec::<String>::new(),
        "the executed switch leaves nothing queued"
    );
    assert!(
        !*daemon
            .pending_switch_off
            .lock()
            .expect("pending_switch_off"),
        "the executed switch-off clears the flag"
    );
}

// ── drains_exhausted: the pure skip predicate ─────────────────────────────────

/// The pure skip predicate, both triggers: a spent tick window (`Some(0)`)
/// skips the next drain, and so does a spent watchdog deadline — whatever the
/// window holds. A tick with neither runs its next drain; `None` (no budget
/// armed, as in a direct drain call from a test) never skips on the window.
#[test]
fn drains_exhausted_names_both_skip_triggers() {
    let t = std::time::Instant::now();
    let deadline = t + Duration::from_secs(29);
    assert!(
        super::drains_exhausted(Some(Duration::ZERO), t, deadline),
        "a spent window skips the next drain"
    );
    assert!(
        !super::drains_exhausted(Some(Duration::from_secs(5)), t, deadline),
        "an unspent window before the deadline does not skip"
    );
    assert!(
        !super::drains_exhausted(None, t, deadline),
        "no budget armed (a direct drain call) does not skip"
    );
    assert!(
        super::drains_exhausted(Some(Duration::from_secs(5)), deadline, deadline),
        "a spent deadline skips whatever the window holds (now == deadline)"
    );
    assert!(
        super::drains_exhausted(
            Some(Duration::from_secs(5)),
            deadline + Duration::from_secs(1),
            deadline
        ),
        "a spent deadline skips whatever the window holds (now past the deadline)"
    );
}

// ── drain_pending_switch ──────────────────────────────────────────────────────

/// A queued auto-switch to an idle, installable target with a clean (non-diverged)
/// active is executed — active becomes the target.
#[test]
fn drain_pending_switch_executes_when_idle_and_clean() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "an idle, clean, installable target must be switched to"
    );
}

/// A SCHEDULER switch is SKIPPED when the outgoing active has unsaved,
/// diverged credentials (a CC re-login / token rotation) — automation may not
/// discard a login clauth doesn't own, so it defers to the operator.
#[test]
fn drain_pending_switch_skips_on_active_divergence() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    // Live ~/.claude token differs from alpha's stored token → Diverged, and alpha
    // has stored creds so it is not a first-login adoption.
    diverge_active("at-alpha-ROTATED");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta", Origin::Scheduler, now_ms() + 120_000);
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "a diverged, unsaved active must block a scheduler switch (no daemon prompt)"
    );
    // TECH-6: deferred, not dropped — the divergence may resolve, so it stays queued.
    assert_eq!(
        queued_targets(&daemon),
        vec!["beta".to_string()],
        "a switch blocked by divergence is re-queued for retry, not silently dropped"
    );
}

/// RESCUE-2: the same diverged state does NOT block a USER switch — the tap is
/// the operator decision the daemon can't prompt for. The unsaved live login is
/// archived into `~/.clauth/quarantine/` (loss-free) and the switch proceeds
/// with discard semantics. Pre-fix, a socket-originated user switch had no path
/// past a foreign live login and wedged until that login died (2026-07-16).
#[test]
fn drain_pending_switch_user_origin_archives_diverged_login_and_proceeds() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    diverge_active("at-alpha-FOREIGN");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "a user switch must proceed past a diverged live login"
    );
    assert!(
        queued_targets(&daemon).is_empty(),
        "a landed switch must not be re-queued"
    );
    let quarantine = clauth_dir().expect("clauth dir").join("quarantine");
    let archived: Vec<_> = std::fs::read_dir(&quarantine)
        .expect("quarantine dir must exist after a discard switch")
        .map(|e| e.expect("dir entry").path())
        .collect();
    assert_eq!(
        archived.len(),
        1,
        "exactly one archived copy of the discarded login"
    );
    let name = archived[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        name.ends_with("-alpha.credentials.json"),
        "archive is named after the outgoing profile: {name}"
    );
    let saved = std::fs::read_to_string(&archived[0]).expect("read archived login");
    assert!(
        saved.contains("at-alpha-FOREIGN"),
        "the archived copy must hold the discarded login's tokens (loss-free)"
    );
    // The live slot now belongs to beta's stored chain.
    let live = crate::claude::read_claude_credentials()
        .expect("read live")
        .expect("live present");
    assert_eq!(
        live.access_token(),
        Some("at-beta"),
        "the live slot must hold the target's stored login after the discard switch"
    );
}

/// Claude Code's logged-out SHELL (both tokens blanked, `expiresAt: 0` — what
/// CC writes when its own refresh dies, keeping unrelated keys like
/// `mcpOAuth`) still classifies Diverged, but holds no login to protect. The
/// queued switch must PROCEED over it — even a headless (Scheduler) switch,
/// which otherwise defers on divergence — because deferring wedged every
/// headless switch behind a TUI decision about an empty file while running
/// sessions sat at "Login expired" (observed 2026-07-15).
#[test]
fn drain_pending_switch_proceeds_over_a_logged_out_shell() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    let dir = claude_dir().expect("claude dir");
    std::fs::create_dir_all(&dir).expect("mkdir ~/.claude");
    let live = dir.join(".credentials.json");
    std::fs::write(
        &live,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "",
                "refreshToken": "",
                "expiresAt": 0,
                "scopes": ["user:inference"],
                "subscriptionType": "max",
            },
            "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
        })
        .to_string(),
    )
    .expect("write live shell");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta", Origin::Scheduler, now_ms() + 120_000);
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "a token-less shell must not block the switch"
    );
    // The shell was replaced by beta's stored login (symlink on unix, copy on
    // Windows — assert through the content, not the link type).
    let installed: ClaudeCredentials =
        crate::profile::read_json_file(&live).expect("read installed live credentials");
    assert_eq!(
        installed.access_token(),
        Some("at-beta"),
        "the live slot now holds the target's stored login"
    );
    // The empty shell was never captured over the outgoing store.
    let alpha_store = crate::profile::profile_dir(&crate::profile::ProfileName::from("alpha"))
        .expect("alpha dir")
        .join("credentials.json");
    let stored: ClaudeCredentials =
        crate::profile::read_json_file(&alpha_store).expect("read alpha store");
    assert_eq!(
        stored.access_token(),
        Some("at-alpha"),
        "the shell's blank tokens must never overwrite the outgoing profile's stored login"
    );
    assert_eq!(
        queued_targets(&daemon),
        Vec::<String>::new(),
        "the executed switch leaves nothing queued"
    );
}

/// A user tap and a scheduler auto-target queued the same tick: the User request
/// wins the drain and the superseded scheduler target is dropped.
#[test]
fn user_switch_outranks_same_tick_auto() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("home", "at-home"),
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("home"),
        90_000,
    );
    link_active_clean("home");
    let mut daemon = daemon_for(config);

    // Stage both directly so they coexist at drain time (drain-side precedence).
    stage_switch(&daemon, "alpha", Origin::Scheduler, now_ms() + 120_000);
    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "User origin outranks a same-tick Scheduler auto-switch"
    );
    assert!(
        queued_targets(&daemon).is_empty(),
        "the superseded scheduler target is dropped, not left queued"
    );
}

/// The enqueue helper's clearing rule: a User request clears any queued Scheduler
/// target on the way in, so only the user's choice remains and lands.
#[test]
fn user_switch_clears_queued_scheduler() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("home", "at-home"),
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("home"),
        90_000,
    );
    link_active_clean("home");
    let mut daemon = daemon_for(config);

    {
        let mut q = daemon.pending_switch.lock().expect("pending_switch");
        enqueue_pending_switch(
            &mut q,
            "alpha".into(),
            crate::profile::Harness::Claude,
            Origin::Scheduler,
            now_ms(),
        );
        enqueue_pending_switch(
            &mut q,
            "beta".into(),
            crate::profile::Harness::Claude,
            Origin::User,
            now_ms(),
        );
    }
    assert_eq!(
        queued_targets(&daemon),
        vec!["beta".to_string()],
        "a user enqueue clears the queued scheduler target"
    );

    daemon.drain_pending_switch();
    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "the user's choice is the one that lands"
    );
}

// ── drain_config_ops: fingerprint-suppression contract ────────────────────────

/// A threshold edit touches only the profile's `config.toml` and returns
/// `Ok(false)`, so `drain_config_ops` must NOT adopt a fresh `last_reload_fp` —
/// an external `profiles.toml` write that landed the same tick still triggers a
/// reload. Pins the exact regression the ledger caught by eyeball.
#[test]
fn drain_config_ops_threshold_does_not_suppress_external_reload() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let mut daemon = daemon_for(config);
    assert_eq!(daemon.refresh_interval.load(Ordering::Relaxed), 90_000);

    // Simulate an unrelated external edit landing this tick: rewrite profiles.toml
    // with a new refresh interval and force its mtime strictly ahead of the
    // daemon's recorded mtime.
    let external = AppState {
        active_profile: Some("alpha".into()),
        profiles: vec!["alpha".into()],
        refresh_interval_ms: 30_000,
        ..AppState::default()
    };
    save_app_state(&external).expect("external app-state write");
    let state_path = clauth_dir().unwrap().join("profiles.toml");
    set_mtime(&state_path, SystemTime::now() + Duration::from_secs(5));

    // A threshold edit (Ok(false)) must leave last_reload_fp untouched.
    daemon
        .pending_config_ops
        .lock()
        .expect("pending_config_ops")
        .push(ConfigOp::SetThreshold("alpha".into(), 50.0));
    daemon.drain_config_ops();

    // Because the threshold edit did not adopt a fresh fingerprint, the reload fires.
    daemon.reload_if_changed();
    assert_eq!(
        daemon.refresh_interval.load(Ordering::Relaxed),
        30_000,
        "a wrote_state=false edit must not swallow a same-tick external reload"
    );
}

// ── reload_if_changed ─────────────────────────────────────────────────────────

// ── TECH-7: cross-process RMW atomicity (lost-update) ─────────────────────────

/// The names in the on-disk `profiles.toml`, freshly reloaded from disk.
fn on_disk_profile_names() -> Vec<String> {
    load_config()
        .expect("reload config")
        .state
        .profiles
        .iter()
        .map(|n| n.to_string())
        .collect()
}

/// A daemon switch must PRESERVE a profile another process appended to
/// `profiles.toml` after the daemon loaded its config — the core lost-update fix
/// (finding #1). Without the reload-merge, `finish_switch`'s blind rewrite from the
/// daemon's stale snapshot would orphan the externally-added profile.
#[test]
fn lost_update_switch_preserves_externally_added_profile() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    // Daemon's in-memory config is [alpha, beta] active=alpha.
    let mut daemon = daemon_for(config);

    // Simulate a concurrent `clauth login gamma`: gamma's dir+creds land on disk and
    // gamma is appended to profiles.toml — but the daemon still predates it (we do
    // NOT reload it, so its snapshot is stale, exactly the race window).
    save_profile(&profile_with_creds("gamma", "at-gamma")).expect("external profile dir");
    let external = AppState {
        active_profile: Some("alpha".into()),
        profiles: vec!["alpha".into(), "beta".into(), "gamma".into()],
        refresh_interval_ms: 90_000,
        ..AppState::default()
    };
    save_app_state(&external).expect("external profiles.toml append");

    // Daemon switches alpha→beta from its STALE snapshot (drain bypasses reload).
    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    daemon.drain_pending_switch();

    let names = on_disk_profile_names();
    assert!(
        names.iter().any(|n| n == "gamma"),
        "externally-added 'gamma' must survive the switch's merged save (got {names:?})"
    );
    assert!(
        names.iter().any(|n| n == "alpha") && names.iter().any(|n| n == "beta"),
        "the daemon's own profiles are still present"
    );
    assert_eq!(
        load_config().unwrap().state.active_profile.as_deref(),
        Some("beta"),
        "the switch itself still applied"
    );
}

// ── TECH-8: switch-event observability + failure backoff ──────────────────────

// ── TECH-9 #13: ~/.clauth 0700 enforcement ────────────────────────────────────

/// An executed switch records the `last_switch` hero event with from/to/trigger.
#[test]
fn successful_switch_records_last_switch_event() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta", Origin::User, now_ms() + 120_000);
    daemon.drain_pending_switch();

    let ls = daemon.last_switch.as_ref().expect("last_switch recorded");
    assert_eq!(ls.from.as_deref(), Some("alpha"), "from = previous active");
    assert_eq!(ls.to.as_deref(), Some("beta"), "to = new active");
    assert_eq!(ls.trigger, "user", "trigger reflects the queue Origin");
}

/// RESCUE-1 test doubles: legs of `follow_live_login_with` a given case must
/// never reach. Panics beat silent misroutes.
fn no_refresh(
    _: &str,
    _: Option<&str>,
) -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError> {
    panic!("the refresh probe must not run in this case")
}
fn no_gate(_: &crate::profile::ProfileName) -> crate::oauth::AuthGate {
    panic!("the install gate must not run in this case")
}

// ── follow_live_login: unattended sibling-divergence self-heal ──────────────
//
// When the ACTIVE profile's live link diverges and the live login PROVABLY
// belongs to a different stored profile, the daemon follows Claude Code there
// (capture + bookkeeping switch) instead of deferring forever with "resolve
// in the TUI". Identity fn injected — no network in tests.

/// Tier 1 (exact token match, no identity call): the live file carries the
/// sibling's exact stored pair — a half-landed switch. The daemon captures it
/// into the sibling and makes the sibling active; the identity fn must never
/// be called.
#[test]
fn follow_live_login_adopts_a_sibling_by_exact_token_match() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    // Live file = beta's EXACT stored credentials, while alpha is active.
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-beta")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|_| panic!("token equality must not need the network"),
        &no_refresh,
        &no_gate,
    );

    assert_eq!(
        active_of(&d).as_deref(),
        Some("beta"),
        "the daemon follows claude code to the owning profile"
    );
    // The persisted state agrees (narrow delta wrote through).
    let disk: AppState = toml::from_str(
        &std::fs::read_to_string(clauth_dir().expect("dir").join("profiles.toml"))
            .expect("read state"),
    )
    .expect("parse state");
    assert_eq!(disk.active_profile.as_deref(), Some("beta"));
}

/// A refresh-less live login (a sibling's rolling-token or static bearer, or
/// a half-landed switch onto its sidecar) tier-1 matches that sibling on the
/// access token. Following it moves the active pointer ONLY: capturing it wrote
/// the refresh-less bearer over the sibling's store and destroyed its refresh
/// chain, unattended (UPS-19 inventory audit, 2026-09-24).
#[test]
fn follow_to_a_refresh_less_sibling_login_keeps_the_siblings_chain() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    // Live file = beta's bearer WITHOUT its refresh token, while alpha is active.
    let mut bearer = oauth_creds("at-beta");
    if let Some(o) = bearer.claude_ai_oauth.as_mut() {
        o.refresh_token = None;
    }
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(&live, serde_json::to_vec(&bearer).expect("ser")).expect("write live");

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|_| panic!("token equality must not need the network"),
        &no_refresh,
        &no_gate,
    );

    assert_eq!(
        active_of(&d).as_deref(),
        Some("beta"),
        "the pointer still follows"
    );
    let stored = crate::profile::load_profile(&crate::profile::ProfileName::from("beta"))
        .expect("load beta");
    assert_eq!(
        stored.refresh_token(),
        Some("rt-at-beta"),
        "beta's stored refresh chain survives the follow"
    );
}

/// Tier 2 (network-verified uuid vs the sibling's cached anchor): a fresh CC
/// re-login into a known account — every token new, identity proven by uuid.
/// The sibling's store adopts the live pair and becomes active.
#[test]
fn follow_live_login_adopts_a_sibling_by_verified_account_uuid() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("beta"),
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-beta".to_string(),
    );
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-fresh-relogin")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|tok| {
            assert_eq!(tok, "at-fresh-relogin");
            crate::usage::IdentityProbe::Proven(crate::usage::AccountIdentity {
                uuid: crate::profile::AccountId::from("uuid-beta".to_string()),
                email: None,
            })
        },
        &no_refresh,
        &no_gate,
    );

    assert_eq!(active_of(&d).as_deref(), Some("beta"));
    // The live pair was captured into beta's store.
    let stored: ClaudeCredentials = crate::profile::read_json_file(
        &crate::profile::profile_dir(&crate::profile::ProfileName::from("beta"))
            .expect("dir")
            .join("credentials.json"),
    )
    .expect("read beta creds");
    assert_eq!(stored.access_token(), Some("at-fresh-relogin"));
}

/// A PROVEN-foreign login (an account clauth holds nowhere) is LEFT ALONE
/// (the TUI's decision) and memoized so the daemon doesn't re-examine (or
/// re-log) the same login every tick. Every stored login is anchored here —
/// the ForeignAccount verdict requires complete coverage (RESCUE-2b).
#[test]
fn follow_live_login_leaves_a_proven_foreign_login_alone_and_memoizes() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("alpha"),
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-alpha".to_string(),
    );
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("beta"),
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-beta".to_string(),
    );
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-foreign")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let identity = |_: &str| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::usage::IdentityProbe::Proven(crate::usage::AccountIdentity {
            uuid: crate::profile::AccountId::from("uuid-nobody-stores".to_string()),
            email: None,
        })
    };
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(active_of(&d).as_deref(), Some("alpha"), "nothing followed");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Same login again: memo short-circuits — no second identity fetch.
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // A NEW live login re-arms the examination.
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-foreign-2")).expect("ser"),
    )
    .expect("write live");
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A login matching the ACTIVE profile's own anchor is the adopt path's
/// domain: follow must stand down (and not spam the log).
#[test]
fn follow_live_login_stands_down_on_a_same_account_divergence() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("alpha"),
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-alpha".to_string(),
    );
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-alpha-rotated")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|_| {
            crate::usage::IdentityProbe::Proven(crate::usage::AccountIdentity {
                uuid: crate::profile::AccountId::from("uuid-alpha".to_string()),
                email: None,
            })
        },
        &no_refresh,
        &no_gate,
    );
    assert_eq!(
        active_of(&d).as_deref(),
        Some("alpha"),
        "same-account divergence is adopt's job, not follow's"
    );
}

/// RESCUE-2b: a PROVEN uuid that matches no anchor proves foreignness only
/// when every stored login HAS an anchor. With coverage incomplete (anchors
/// are dropped on unproven re-logins and backfilled by the next /profile
/// poll), the live login could still be an owned account — so it retries on
/// the timer instead of memoizing "resolve in the TUI" for good.
#[test]
fn follow_unmatched_proven_login_with_unanchored_profiles_retries_not_memoizes() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    // alpha anchored; beta holds a login but has NO anchor → incomplete coverage.
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("alpha"),
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-alpha".to_string(),
    );
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-mystery")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let identity = |_: &str| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::usage::IdentityProbe::Proven(crate::usage::AccountIdentity {
            uuid: crate::profile::AccountId::from("uuid-matches-no-anchor".to_string()),
            email: None,
        })
    };
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(active_of(&d).as_deref(), Some("alpha"), "nothing followed");
    assert_eq!(
        d.follow_memo, None,
        "incomplete anchor coverage must not memoize a possibly-owned login as foreign"
    );
    assert!(d.follow_retry_at > 0, "retries on the timer instead");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Inside the window: quiet. Window elapsed: examined again.
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    d.follow_retry_at = 0;
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// RESCUE-2b: a sibling adoption whose local WRITE fails (here: a read-only
/// profile dir) arms the retry timer — it spent nothing, so it must not be
/// memoized against the login (the pre-RESCUE-2 behavior wedged a
/// legitimately owned login behind one transient local error for good).
#[cfg(unix)]
#[test]
fn follow_sibling_overwrite_failure_arms_the_retry_timer_not_the_memo() {
    use std::os::unix::fs::PermissionsExt;
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    // Tier-1 exact-token match: the live login IS beta's stored chain
    // (a half-landed switch) — no network needed to attribute it.
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-beta")).expect("ser"),
    )
    .expect("write live");
    let beta_dir =
        crate::profile::profile_dir(&crate::profile::ProfileName::from("beta")).expect("beta dir");

    let mut d = daemon_for(config);
    std::fs::set_permissions(&beta_dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    d.follow_live_login_with(
        &|_| panic!("token equality must not need the network"),
        &no_refresh,
        &no_gate,
    );
    std::fs::set_permissions(&beta_dir, std::fs::Permissions::from_mode(0o755)).expect("restore");

    assert_eq!(
        active_of(&d).as_deref(),
        Some("alpha"),
        "the failed adoption left the active profile unchanged"
    );
    assert_eq!(
        d.follow_memo, None,
        "a local write failure is never memoized against the login"
    );
    assert!(d.follow_retry_at > 0, "it retries on the timer");

    // Window elapsed + writable again: the adoption completes.
    d.follow_retry_at = 0;
    d.follow_live_login_with(
        &|_| panic!("token equality must not need the network"),
        &no_refresh,
        &no_gate,
    );
    assert_eq!(
        active_of(&d).as_deref(),
        Some("beta"),
        "the retry lands once the transient failure clears"
    );
}

// ── RESCUE-1: dead-live-login reclaim ────────────────────────────────────────
//
// A diverged live login the endpoint CONFIRMS dead protects nothing — the
// daemon reclaims the live slot with the active profile's stored chain instead
// of wedging every switch behind "resolve in the TUI" while the running
// `claude` stays signed out. Probes injected — no network in tests.

/// Writes a raw live-credentials JSON (regular file) and returns its path.
fn write_live_json(json: &serde_json::Value) -> std::path::PathBuf {
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(&live, serde_json::to_vec(json).expect("ser")).expect("write live");
    live
}

fn dead_probe(_: &str) -> crate::usage::IdentityProbe {
    crate::usage::IdentityProbe::Rejected
}

fn refresh_confirms_dead(
    _: &str,
    _: Option<&str>,
) -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError> {
    Err(crate::oauth::RefreshError::Invalid(
        crate::oauth::TokenFailure::Status(400),
    ))
}

fn gate_ready(_: &crate::profile::ProfileName) -> crate::oauth::AuthGate {
    crate::oauth::AuthGate::Ready
}

/// Endpoint-confirmed dead (identity 401 + refresh `invalid_grant`) + a healthy
/// stored chain → the live slot is reclaimed: the file becomes the active
/// profile's symlink again, and the memo/backoff clear so the next divergence
/// is examined fresh.
#[test]
fn rescue_reclaims_an_endpoint_confirmed_dead_live_login() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    d.follow_live_login_with(&dead_probe, &refresh_confirms_dead, &gate_ready);

    let meta = live.symlink_metadata().expect("live exists");
    assert!(
        meta.file_type().is_symlink(),
        "the corpse is replaced by the active profile's link"
    );
    let target = std::fs::read_link(&live).expect("readlink");
    assert!(
        target.ends_with("alpha/credentials.json"),
        "relinked to the ACTIVE profile's stored chain: {}",
        target.display()
    );
    assert_eq!(active_of(&d).as_deref(), Some("alpha"), "active unchanged");
    assert_eq!(d.follow_memo, None, "no memo left against the dead login");
    assert_eq!(
        d.follow_retry_at, 0,
        "no backoff left after a completed rescue"
    );
}

/// `AuthGate::Refreshed` (the stored chain was expiring and the gate rotated
/// it on the way in) shares the reclaim's pass-arm with `Ready` — the freshly
/// rotated chain must still take the slot. Pins the coalescing: sibling call
/// sites already treat Refreshed distinctly (the TUI's switch path), so a
/// future split of this match must not silently drop the reclaim.
#[test]
fn rescue_reclaims_through_a_refreshed_install_gate() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    let gate_refreshed = |_: &crate::profile::ProfileName| crate::oauth::AuthGate::Refreshed;
    d.follow_live_login_with(&dead_probe, &refresh_confirms_dead, &gate_refreshed);

    assert!(
        live.symlink_metadata()
            .expect("live exists")
            .file_type()
            .is_symlink(),
        "a Refreshed gate must reclaim exactly like Ready"
    );
    assert_eq!(d.follow_memo, None);
    assert_eq!(d.follow_retry_at, 0);
}

/// The identity endpoint rejected the access token but the refresh leg only
/// failed transiently: proves nothing — no reclaim, and the network tier backs
/// off on the timer instead of re-probing every tick (or memoizing for good).
#[test]
fn rescue_backs_off_when_the_refresh_leg_is_transient() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    let probes = std::sync::atomic::AtomicUsize::new(0);
    let identity = |_: &str| {
        probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::usage::IdentityProbe::Rejected
    };
    let transient =
        |_: &str,
         _: Option<&str>|
         -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError> {
            Err(crate::oauth::RefreshError::Transient(
                crate::oauth::TokenFailure::Transport,
            ))
        };
    d.follow_live_login_with(&identity, &transient, &no_gate);

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "no reclaim on an unconfirmed death"
    );
    assert!(d.follow_retry_at > 0, "a retry window is armed");
    assert_eq!(d.follow_memo, None, "a transient outcome is never memoized");

    // Inside the window: the network tier stays quiet (tier 1 still runs).
    d.follow_live_login_with(&identity, &transient, &no_gate);
    assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Window elapsed: the probe re-runs.
    d.follow_retry_at = 0;
    d.follow_live_login_with(&identity, &transient, &no_gate);
    assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// The refresh probe SUCCEEDED — the pair was alive (the identity 401 was the
/// access token dying of old age). The probe consumed the file's single-use
/// refresh token, so the fresh pair lands straight back in the live file —
/// preserving every foreign top-level key (CC parks `mcpOAuth` there) — and
/// nothing is reclaimed.
#[test]
fn rescue_writes_a_still_alive_pair_back_in_place() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "at-stale",
            "refreshToken": "rt-stale",
            "expiresAt": 0,
            "scopes": ["user:inference"],
            "subscriptionType": "max",
        },
        "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
    }));

    let mut d = daemon_for(config);
    let refresh_ok =
        |rt: &str,
         scopes: Option<&str>|
         -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError> {
            assert_eq!(rt, "rt-stale", "must spend the live file's refresh token");
            assert_eq!(scopes, Some("user:inference"), "must carry the live scopes");
            Ok(crate::oauth::TokenResponse {
                access_token: "at-rotated".to_string(),
                refresh_token: "rt-rotated".to_string(),
                expires_in: 28_800,
                scope: None,
            })
        };
    d.follow_live_login_with(&dead_probe, &refresh_ok, &no_gate);

    let meta = live.symlink_metadata().expect("live");
    assert!(
        !meta.file_type().is_symlink(),
        "an alive login is never reclaimed"
    );
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&live).expect("read")).expect("json");
    assert_eq!(v["claudeAiOauth"]["accessToken"], "at-rotated");
    assert_eq!(v["claudeAiOauth"]["refreshToken"], "rt-rotated");
    assert!(
        v["claudeAiOauth"]["expiresAt"].as_u64().expect("ms") > crate::usage::now_ms(),
        "expiry re-stamped in the future"
    );
    assert_eq!(
        v["claudeAiOauth"]["subscriptionType"], "max",
        "untouched oauth fields survive"
    );
    assert_eq!(
        v["mcpOAuth"]["some-server"]["accessToken"], "mcp-tok",
        "foreign top-level keys survive the surgical write"
    );
    assert!(
        d.follow_retry_at > 0,
        "the follow-up re-identification waits out the probe window — an instant \
         retry would let a pathologically still-401ing token refresh-storm"
    );
}

/// Confirmed-dead live login but the active's STORED chain is broken too:
/// nothing installable may take the slot (AUTH-1) — no reclaim, retry armed.
#[test]
fn rescue_refuses_when_the_stored_chain_is_broken_too() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    let gate_broken = |_: &crate::profile::ProfileName| crate::oauth::AuthGate::Broken;
    d.follow_live_login_with(&dead_probe, &refresh_confirms_dead, &gate_broken);

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "a dead stored chain must never take the live slot"
    );
    assert!(d.follow_retry_at > 0, "retries once a re-login lands");
}

/// A live login whose access token is rejected and which carries NO refresh
/// token is unusable by anyone — reclaimed directly, no refresh leg to consult.
#[test]
fn rescue_reclaims_a_refreshless_rejected_login_directly() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": { "accessToken": "at-dead", "expiresAt": 0 },
    }));

    let mut d = daemon_for(config);
    d.follow_live_login_with(&dead_probe, &no_refresh, &gate_ready);

    assert!(
        live.symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "nothing to preserve, nothing to confirm — reclaimed"
    );
}

/// Concurrent-write guard: fresh credentials landing between the probe and the
/// reclaim (a CC-side re-login or refresh) must never be overwritten. The
/// refresh leg simulates the race by rewriting the live file before returning
/// its confirmed-dead verdict.
#[test]
fn rescue_aborts_when_the_live_login_changes_mid_probe() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    let live_for_closure = live.clone();
    let racing_refresh = move |_: &str,
                               _: Option<&str>|
          -> std::result::Result<
        crate::oauth::TokenResponse,
        crate::oauth::RefreshError,
    > {
        // A concurrent CC re-login replaces the file mid-probe.
        std::fs::write(
            &live_for_closure,
            serde_json::to_vec(&oauth_creds("at-brand-new-login")).expect("ser"),
        )
        .expect("racing write");
        Err(crate::oauth::RefreshError::Invalid(
            crate::oauth::TokenFailure::Status(400),
        ))
    };
    d.follow_live_login_with(&dead_probe, &racing_refresh, &gate_ready);

    let meta = live.symlink_metadata().expect("live");
    assert!(
        !meta.file_type().is_symlink(),
        "the freshly landed login must survive the aborted reclaim"
    );
    let survived: ClaudeCredentials =
        crate::profile::read_json_file(&live).expect("read survived login");
    assert_eq!(
        survived.access_token(),
        Some("at-brand-new-login"),
        "the racing login's bytes are untouched"
    );
}

/// A probe OUTAGE (Indeterminate — the 2026-07-14 incident class) must never
/// reclaim, never memoize, and retry on the timer. This is the central
/// RESCUE-1 guarantee: memoizing one bad probe against the login is what
/// wedged the daemon for a day.
#[test]
fn follow_probe_outage_is_never_memoized_and_retries_on_the_timer() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-mystery")).expect("val"));

    let mut d = daemon_for(config);
    let probes = std::sync::atomic::AtomicUsize::new(0);
    let outage = |_: &str| {
        probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::usage::IdentityProbe::Indeterminate
    };
    d.follow_live_login_with(&outage, &no_refresh, &no_gate);

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "an unproven login is never reclaimed"
    );
    assert_eq!(
        d.follow_memo, None,
        "a probe outage is NEVER memoized against the login"
    );
    assert!(d.follow_retry_at > 0, "the retry window is armed");
    assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Inside the window: quiet (tier 1 only).
    d.follow_live_login_with(&outage, &no_refresh, &no_gate);
    assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Window elapsed: probed again — the outage was not terminal.
    d.follow_retry_at = 0;
    d.follow_live_login_with(&outage, &no_refresh, &no_gate);
    assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// Write-back leg of the mid-probe race: the refresh SUCCEEDS but a fresh CC
/// login landed during the roundtrip. The rotated pair must be discarded (it
/// continues the corpse's superseded lineage) — writing it would destroy the
/// fresh login's only refresh-token copy.
#[test]
fn rescue_write_back_aborts_when_a_fresh_login_lands_mid_refresh() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    let live_for_closure = live.clone();
    let racing_refresh = move |_: &str,
                               _: Option<&str>|
          -> std::result::Result<
        crate::oauth::TokenResponse,
        crate::oauth::RefreshError,
    > {
        std::fs::write(
            &live_for_closure,
            serde_json::to_vec(&oauth_creds("at-brand-new-login")).expect("ser"),
        )
        .expect("racing write");
        Ok(crate::oauth::TokenResponse {
            access_token: "at-rotated".to_string(),
            refresh_token: "rt-rotated".to_string(),
            expires_in: 28_800,
            scope: None,
        })
    };
    d.follow_live_login_with(&dead_probe, &racing_refresh, &no_gate);

    let survived: ClaudeCredentials =
        crate::profile::read_json_file(&live).expect("read survived login");
    assert_eq!(
        survived.access_token(),
        Some("at-brand-new-login"),
        "the freshly landed login must never be clobbered by the discarded rotation"
    );
}

/// Reclaim FAILURE (gate passed, fingerprint matched, but the relink itself
/// errored): arms the retry timer — never memoizes, never wedges.
#[cfg(unix)]
#[test]
fn rescue_reclaim_failure_arms_the_retry_timer() {
    use std::os::unix::fs::PermissionsExt;
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));
    let claude = live.parent().expect("claude dir").to_path_buf();

    let mut d = daemon_for(config);
    // Read-only ~/.claude: force_link's remove_file fails after every gate passed.
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    d.follow_live_login_with(&dead_probe, &refresh_confirms_dead, &gate_ready);
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).expect("restore");

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "the failed reclaim left the corpse in place"
    );
    assert!(
        d.follow_retry_at > 0,
        "a failed reclaim retries on the timer"
    );
    assert_eq!(d.follow_memo, None, "a failed reclaim is never memoized");
}

/// Write-back FAILURE (refresh succeeded — the single-use refresh token is
/// spent — but the rotated pair could not be persisted): the live chain is
/// lost through our own probe. Memoized + named loudly; only a re-login
/// recovers, so retrying would just spend more tokens.
#[cfg(unix)]
#[test]
fn rescue_write_back_failure_memoizes_the_lost_chain() {
    use std::os::unix::fs::PermissionsExt;
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));
    let claude = live.parent().expect("claude dir").to_path_buf();

    let mut d = daemon_for(config);
    let claude_for_closure = claude.clone();
    // The dir turns read-only mid-rescue: the fingerprint re-read still works
    // (read allowed) but the atomic write of the rotated pair cannot land.
    let refresh_then_lock = move |_: &str,
                                  _: Option<&str>|
          -> std::result::Result<
        crate::oauth::TokenResponse,
        crate::oauth::RefreshError,
    > {
        std::fs::set_permissions(&claude_for_closure, std::fs::Permissions::from_mode(0o555))
            .expect("chmod");
        Ok(crate::oauth::TokenResponse {
            access_token: "at-rotated".to_string(),
            refresh_token: "rt-rotated".to_string(),
            expires_in: 28_800,
            scope: None,
        })
    };
    d.follow_live_login_with(&dead_probe, &refresh_then_lock, &no_gate);
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).expect("restore");

    assert!(
        d.follow_memo.is_some(),
        "a lost chain is memoized — only a re-login recovers, retrying spends more tokens"
    );
}

/// Stored chain refreshes transiently mid-reclaim (gate Transient): no reclaim,
/// retry armed — a network blip on OUR side must not destroy the corpse early.
#[test]
fn rescue_gate_transient_arms_the_retry_timer() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::to_value(oauth_creds("at-dead")).expect("val"));

    let mut d = daemon_for(config);
    let gate_transient = |_: &crate::profile::ProfileName| {
        crate::oauth::AuthGate::Transient(crate::format::Transient::new(
            crate::format::Cause::Endpoint("blip"),
            crate::format::Retry::Wait,
        ))
    };
    d.follow_live_login_with(&dead_probe, &refresh_confirms_dead, &gate_transient);

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "no reclaim behind a transient gate"
    );
    assert!(d.follow_retry_at > 0);
    assert_eq!(d.follow_memo, None);
}

/// A Proven identity with a BLANK uuid is shape drift, not an identity: it must
/// read as unproven (timed retry), never as a proven-foreign account (memo).
#[test]
fn follow_blank_uuid_reads_as_unproven_not_foreign() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    write_live_json(&serde_json::to_value(oauth_creds("at-blank")).expect("val"));

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|_| {
            crate::usage::IdentityProbe::Proven(crate::usage::AccountIdentity {
                uuid: crate::profile::AccountId::from("   ".to_string()),
                email: None,
            })
        },
        &no_refresh,
        &no_gate,
    );
    assert_eq!(
        d.follow_memo, None,
        "two blanks must never prove an identity"
    );
    assert!(d.follow_retry_at > 0, "unproven retries on the timer");
}

/// A foreign login observed while the probe window is closed reads as unproven
/// — the identity fn must not run, and nothing is memoized (the real verdict
/// waits for the window).
#[test]
fn follow_foreign_login_inside_backoff_is_not_memoized() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    write_live_json(&serde_json::to_value(oauth_creds("at-foreign")).expect("val"));

    let mut d = daemon_for(config);
    d.follow_retry_at = crate::usage::now_ms() + 600_000;
    d.follow_live_login_with(
        &|_| panic!("the identity probe must not run inside the backoff window"),
        &no_refresh,
        &no_gate,
    );
    assert_eq!(d.follow_memo, None, "no verdict, no memo");
}

/// RESCUE-1b: Claude Code's logged-out SHELL (blanked tokens, expiresAt 0 —
/// what CC writes when its own refresh dies) is not a login at all. It must be
/// reclaimed on sight instead of wedging switches behind a TUI decision about
/// nothing — the exact live state observed on 2026-07-15.
#[test]
fn follow_reclaims_a_logged_out_live_shell() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "",
            "refreshToken": "",
            "expiresAt": 0,
            "scopes": ["user:inference"],
            "subscriptionType": "max",
        },
        "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
    }));

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|_| panic!("a token-less shell has nothing to probe"),
        &no_refresh,
        &gate_ready,
    );

    let meta = live.symlink_metadata().expect("live");
    assert!(
        meta.file_type().is_symlink(),
        "the shell is replaced by the active profile's link"
    );
    let target = std::fs::read_link(&live).expect("readlink");
    assert!(target.ends_with("alpha/credentials.json"));
    assert_eq!(d.follow_memo, None);
    assert_eq!(d.follow_retry_at, 0);
}

/// A file with NO OAuth block at all (only foreign keys like mcpOAuth) is the
/// same shell — reclaimed, not deferred.
#[test]
fn follow_reclaims_a_live_file_with_no_oauth_block() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "mcpOAuth": { "some-server": { "accessToken": "mcp-tok" } },
    }));

    let mut d = daemon_for(config);
    d.follow_live_login_with(&|_| panic!("no token to probe"), &no_refresh, &gate_ready);
    assert!(
        live.symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "an OAuth-less live file is a shell — reclaimed"
    );
}

/// Shell + a broken stored chain: nothing installable may take the slot
/// (AUTH-1). No reclaim, and the retry timer keeps the install gate from
/// re-running every tick.
#[test]
fn follow_logged_out_shell_with_broken_store_waits_on_the_timer() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": { "accessToken": "", "refreshToken": "", "expiresAt": 0 },
    }));

    let mut d = daemon_for(config);
    let gates = std::sync::atomic::AtomicUsize::new(0);
    let gate_broken = |_: &crate::profile::ProfileName| {
        gates.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Broken
    };
    d.follow_live_login_with(&|_| panic!("nothing to probe"), &no_refresh, &gate_broken);

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "a dead stored chain must never take the live slot"
    );
    assert!(d.follow_retry_at > 0);
    assert_eq!(gates.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Inside the window the gate is not re-run.
    d.follow_live_login_with(&|_| panic!("nothing to probe"), &no_refresh, &gate_broken);
    assert_eq!(gates.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Shell race: a REAL login lands (CC-side /login) between the shell judgment
/// and the relink — the late still-unchanged re-check must abort the reclaim.
#[test]
fn follow_logged_out_shell_race_aborts_the_reclaim() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": { "accessToken": "", "refreshToken": "", "expiresAt": 0 },
    }));

    let mut d = daemon_for(config);
    let live_for_closure = live.clone();
    let racing_gate = move |_: &crate::profile::ProfileName| {
        // A concurrent CC /login lands a fresh pair mid-reclaim.
        std::fs::write(
            &live_for_closure,
            serde_json::to_vec(&oauth_creds("at-fresh-cc-login")).expect("ser"),
        )
        .expect("racing write");
        crate::oauth::AuthGate::Ready
    };
    d.follow_live_login_with(
        &|_: &str| panic!("nothing to probe"),
        &no_refresh,
        &racing_gate,
    );

    let survived: ClaudeCredentials =
        crate::profile::read_json_file(&live).expect("read survived login");
    assert_eq!(
        survived.access_token(),
        Some("at-fresh-cc-login"),
        "the freshly landed login must survive the aborted shell reclaim"
    );
}

/// RESCUE-2b mixed state: blank access token but the live refresh token
/// byte-matches the ACTIVE profile's own stored chain — a degraded copy of the
/// same chain (torn write). Relinking loses nothing: reclaimed like a shell.
#[test]
fn follow_reclaims_a_degraded_copy_of_the_active_chain() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    // oauth_creds seeds alpha's stored refresh token as "rt-at-alpha".
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": { "accessToken": "", "refreshToken": "rt-at-alpha", "expiresAt": 0 },
    }));

    let mut d = daemon_for(config);
    d.follow_live_login_with(
        &|_| panic!("a token-less file has nothing to probe"),
        &no_refresh,
        &gate_ready,
    );

    assert!(
        live.symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "a degraded copy of the active profile's own chain is reclaimed"
    );
    assert_eq!(d.follow_memo, None);
    assert_eq!(d.follow_retry_at, 0);
}

/// RESCUE-2b mixed state, foreign flavor: blank access token but an
/// UNRECOGNIZED refresh token. There may be a recoverable login in it, so it
/// is left alone — but visibly, on the retry timer, instead of the silent
/// per-tick no-op this state used to be.
#[test]
fn follow_leaves_an_unrecognized_refresh_only_file_alone_on_the_timer() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = write_live_json(&serde_json::json!({
        "claudeAiOauth": { "accessToken": "", "refreshToken": "rt-nobody-stores", "expiresAt": 0 },
    }));

    let mut d = daemon_for(config);
    let gates = std::sync::atomic::AtomicUsize::new(0);
    let counting_gate = |_: &crate::profile::ProfileName| {
        gates.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::oauth::AuthGate::Ready
    };
    d.follow_live_login_with(
        &|_: &str| panic!("no access token to probe"),
        &no_refresh,
        &counting_gate,
    );

    assert!(
        !live
            .symlink_metadata()
            .expect("live")
            .file_type()
            .is_symlink(),
        "an unrecognized refresh token is never clobbered"
    );
    assert_eq!(
        gates.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no reclaim attempted — the gate is never consulted"
    );
    assert_eq!(d.follow_memo, None, "not memoized (the state may resolve)");
    assert!(
        d.follow_retry_at > 0,
        "re-examined on the timer, not per tick"
    );

    // Inside the window: fully quiet.
    d.follow_live_login_with(&|_: &str| panic!("gated"), &no_refresh, &counting_gate);
    assert_eq!(gates.load(std::sync::atomic::Ordering::SeqCst), 0);
}

// ── RESCUE-2b: follow state survives a daemon restart ────────────────────────

/// The 30-min network backoff is persisted: a daemon restart (launchd respawn,
/// `pkill` deploy) inside the window must NOT re-arm the network tier — that
/// was how a restart loop could re-probe (or re-spend a single-use refresh
/// token) once per boot.
#[test]
fn follow_retry_backoff_survives_a_daemon_restart() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-mystery")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    let outage = |_: &str| crate::usage::IdentityProbe::Indeterminate;
    d.follow_live_login_with(&outage, &no_refresh, &no_gate);
    let armed = d.follow_retry_at;
    assert!(armed > now_ms(), "the outage armed the backoff");

    // "Restart": a fresh Daemon over the same sandbox.
    let config2 = crate::profile::load_config().expect("reload config");
    let mut d2 = daemon_for(config2);
    assert_eq!(
        d2.follow_retry_at, armed,
        "the armed backoff survives the restart"
    );
    d2.follow_live_login_with(
        &|_| panic!("the network tier must stay gated across a restart"),
        &no_refresh,
        &no_gate,
    );
}

/// The proven-foreign memo is persisted too: a restart must not re-probe (and
/// re-log "resolve in the TUI" for) the same foreign login on every boot.
#[test]
fn follow_foreign_memo_survives_a_daemon_restart() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "at-alpha")],
        Some("alpha"),
        90_000,
    );
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("alpha"),
        crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
        &"uuid-alpha".to_string(),
    );
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    std::fs::create_dir_all(live.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &live,
        serde_json::to_vec(&oauth_creds("at-foreign")).expect("ser"),
    )
    .expect("write live");

    let mut d = daemon_for(config);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let identity = |_: &str| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::usage::IdentityProbe::Proven(crate::usage::AccountIdentity {
            uuid: crate::profile::AccountId::from("uuid-nobody-stores".to_string()),
            email: None,
        })
    };
    d.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(d.follow_memo.is_some(), "proven-foreign memoized");

    let config2 = crate::profile::load_config().expect("reload config");
    let mut d2 = daemon_for(config2);
    assert_eq!(
        d2.follow_memo, d.follow_memo,
        "the memo survives the restart"
    );
    d2.follow_live_login_with(&identity, &no_refresh, &no_gate);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the same foreign login is not re-probed after a restart"
    );
}

// ── CAP-1 tripwire: duplicate stored logins are named ────────────────────────

/// Two profiles storing byte-identical access tokens = one was captured over
/// with the other's chain (they now double-poll ONE account). The pure pair
/// detector is the daemon's per-tick tripwire; the warning memoizes on the
/// pair-set fingerprint so it logs once per distinct state, not per tick.
#[test]
fn duplicate_stored_logins_are_paired() {
    let _home = HomeSandbox::new();
    let with_token = |name: &str, tok: &str| {
        let mut p = blank_profile(&crate::profile::ProfileName::from(name));
        p.credentials = Some(crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: tok.to_string(),
                refresh_token: Some(format!("{tok}-refresh")),
                expires_at: None,
                scopes: None,
                subscription_type: None,
                ..crate::profile::OAuthToken::default_extra()
            }),
        });
        p
    };
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![
            with_token("a", "shared-token"),
            with_token("b", "unique-token"),
            with_token("c", "shared-token"),
            blank_profile(&crate::profile::ProfileName::from("no-creds")),
        ],
    };

    assert_eq!(
        super::tick::duplicate_login_pairs(&config),
        vec![("a".to_string(), "c".to_string())],
        "the first holder is named alongside each duplicate",
    );

    let clean = AppConfig {
        state: AppState::default(),
        profiles: vec![with_token("a", "t1"), with_token("b", "t2")],
    };
    assert!(
        super::tick::duplicate_login_pairs(&clean).is_empty(),
        "distinct chains raise nothing",
    );
}

/// CAP-2 tripwire: two profiles anchored to the same ACCOUNT under DIFFERENT
/// tokens (a re-login minted the wrong account — the 2026-07-12 recurrence)
/// double-poll it exactly like a copied chain, but the byte-identical check is
/// blind to it. The anchor-pair detector names it.
#[test]
fn duplicate_account_anchors_are_paired() {
    use crate::profile_cache::{ACCOUNT_ID_CACHE_FILE, write_profile_cache};
    let _home = HomeSandbox::new();
    let names: Vec<String> = ["a", "b", "c", "blank-1", "blank-2"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // The cache writes below are gated on the on-disk record.
    crate::testutil::register_names(&["a", "b", "c", "blank-1", "blank-2"]);
    let pn = crate::profile::ProfileName::from;
    write_profile_cache(&pn("a"), ACCOUNT_ID_CACHE_FILE, &"acct-1".to_string());
    write_profile_cache(&pn("b"), ACCOUNT_ID_CACHE_FILE, &"acct-2".to_string());
    write_profile_cache(&pn("c"), ACCOUNT_ID_CACHE_FILE, &"acct-1".to_string());
    // TWO whitespace-only anchors: shape drift, not identities — absent the
    // is_empty guard they would trim equal and pair, so this fixture is what
    // actually locks the guard in (same contract as fetch_account_uuid).
    write_profile_cache(&pn("blank-1"), ACCOUNT_ID_CACHE_FILE, &"  ".to_string());
    write_profile_cache(&pn("blank-2"), ACCOUNT_ID_CACHE_FILE, &" ".to_string());

    assert_eq!(
        super::tick::duplicate_account_pairs(&names),
        vec![("a".to_string(), "c".to_string())],
        "the first holder is named alongside each same-account duplicate; blanks never pair",
    );

    write_profile_cache(&pn("c"), ACCOUNT_ID_CACHE_FILE, &"acct-3".to_string());
    assert!(
        super::tick::duplicate_account_pairs(&names[..3]).is_empty(),
        "distinct accounts raise nothing",
    );
}

/// A byte-identical pair is ALSO anchor-identical once the backfill runs — the
/// warn path must report it once (under the sharper token message), never
/// twice. Locks in the `account_only_pairs` filter the commit message promises.
#[test]
fn token_pairs_are_filtered_from_the_account_report() {
    let token_pairs = vec![("a".to_string(), "c".to_string())];
    let account_pairs = vec![
        ("a".to_string(), "c".to_string()), // copied chain: in BOTH detectors
        ("d".to_string(), "e".to_string()), // wrong-account re-login: anchors only
    ];
    assert_eq!(
        super::tick::account_only_pairs(&token_pairs, account_pairs),
        vec![("d".to_string(), "e".to_string())],
        "the token-identical pair reports once, under the token message",
    );
    assert_eq!(
        super::tick::account_only_pairs(&[], vec![("a".into(), "c".into())]),
        vec![("a".to_string(), "c".to_string())],
        "no token pairs → account pairs pass through",
    );
}

// ── uncapped_spenders (boot-time warning's pure collection) ───────────────────

/// A disabled member is never spend-armed by the walk, so it must never be
/// named in the "can spend with no cap" warning — only a live, enabled
/// uncapped sibling should surface.
#[test]
fn uncapped_spenders_excludes_disabled_includes_enabled_sibling() {
    let mut disabled = blank_profile(&crate::profile::ProfileName::from("off"));
    disabled.max_auto_spend = Some(5.0);
    disabled.disabled = true;
    let mut enabled = blank_profile(&crate::profile::ProfileName::from("on"));
    enabled.max_auto_spend = Some(5.0);

    let config = AppConfig {
        state: AppState {
            fallback_chain: vec!["off".into(), "on".into()],
            spend_budget_switching: true,
            switch_off_when_budget_spent: false,
            ..AppState::default()
        },
        profiles: vec![disabled, enabled],
    };

    let names = super::uncapped_spenders(&config);
    assert!(
        !names.contains(&"off"),
        "a disabled member must never be named as an uncapped spender"
    );
    assert!(
        names.contains(&"on"),
        "an enabled uncapped sibling must still be named"
    );
}

/// The standby arm tightens the tree BEFORE it parks, never after the takeover.
/// launchd creates `daemon.log` at the umask (0o644) before exec and a park is
/// unbounded in time, so a walk deferred to the promotion leaves a
/// world-readable log naming accounts for the whole wait.
#[cfg(unix)]
#[test]
fn stand_by_tightens_the_tree_before_parking_not_after_promotion() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    // A loose dir standing in for the log: the pre-park walk tightens it.
    let loose = dir.join("loose");
    std::fs::create_dir_all(&loose).expect("mkdir loose");
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // Stand in for the running daemon so the claim below really parks.
    let held = crate::profile::open_state_file(&dir.join(super::LOCK_FILE)).expect("open lock");
    held.try_lock().expect("hold the singleton lock");

    let super::Claim::Standby(slot) = super::claim_singleton(&dir, true).expect("claim") else {
        panic!("the second instance takes the one standby slot");
    };
    let parked = std::thread::spawn({
        let dir = dir.clone();
        move || super::stand_by(&dir, slot)
    });

    let mode = |p: &std::path::Path| {
        std::fs::metadata(p)
            .expect("stat loose")
            .permissions()
            .mode()
            & 0o777
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while mode(&loose) != 0o700 {
        assert!(
            !parked.is_finished(),
            "stand_by returned instead of parking"
        );
        assert!(
            Instant::now() < deadline,
            "the tree stayed 0o755 across 5s of parking: the walk runs only after the promotion, \
             so a standby's whole wait sits in a world-readable tree"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    // Holder exits → the standby takes over.
    drop(held);
    let promoted = parked
        .join()
        .expect("stand_by thread")
        .expect("the standby promotes once the holder exits");
    drop(promoted);
}

/// A `clauth daemon` that loses the singleton race must exit having touched
/// nothing shared. The pile-up in #57 was 25 of these, each having already run
/// the runtime GC and the tree-wide chmod walk against the live daemon's state
/// before parking forever.
#[test]
fn a_redundant_instance_exits_without_touching_the_shared_tree() {
    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");

    // A runtime tree with no live session: `gc_stale_runtimes` deletes it.
    let ghost = dir.join("profiles").join("ghost").join("runtime");
    std::fs::create_dir_all(&ghost).expect("mkdir ghost runtime");
    // A loose dir: `migrate_clauth_perms_700` tightens it to 0o700.
    let loose = dir.join("loose");
    std::fs::create_dir_all(&loose).expect("mkdir loose");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    // Stand in for the running daemon: hold the singleton lock for the call.
    let held = crate::profile::open_state_file(&dir.join(super::LOCK_FILE)).expect("open lock");
    held.try_lock().expect("hold the singleton lock");

    super::serve(
        super::StartMode::ExitIfRunning,
        None,
        &super::api::tls::CertSource::Lego,
    )
    .expect("a redundant instance exits clean");

    assert!(
        ghost.exists(),
        "the redundant instance ran the runtime GC against the live daemon's tree"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&loose)
            .expect("stat loose")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o755,
            "the redundant instance walked the tree's modes before exiting"
        );
    }
}

/// The default's redundant line names the holder's pid so a `ps` dump ties back
/// to it; `--standby` reports a full queue instead. Pins the operator-facing
/// wording (`serve` logs it and exits, which a test can't easily capture).
#[test]
fn redundant_reason_names_the_pid_for_the_default_and_the_queue_for_standby() {
    let _home = HomeSandbox::new();
    let dir = clauth_dir().expect("clauth dir");
    std::fs::create_dir_all(&dir).expect("mkdir");

    // No pid sidecar staged: the default still reads "already running", pid unknown.
    let default = super::redundant_reason(super::StartMode::ExitIfRunning);
    assert!(
        default.starts_with("already running (pid "),
        "the default's redundant reason must read 'already running (pid …)', got {default:?}"
    );

    // With a pid stamped, it surfaces the number.
    std::fs::write(dir.join(super::PID_FILE), "4242\n").expect("stamp pid");
    let with_pid = super::redundant_reason(super::StartMode::ExitIfRunning);
    assert!(
        with_pid.contains("4242"),
        "the default's redundant reason must name the holder pid, got {with_pid:?}"
    );

    let standby = super::redundant_reason(super::StartMode::Standby);
    assert!(
        standby.contains("standby"),
        "the --standby redundant reason must mention the full queue, got {standby:?}"
    );
}

// ── stale-config persist gates (lock-race row 3) ─────────────────────────────

/// A queued switch whose target is deleted out-of-process AFTER the daemon's
/// in-memory config was loaded must be dropped on the fresh membership read,
/// not switched to. The pre-fix drain read the vanished guard off the in-memory
/// list, so the delete (landing between the tick's reload and the drain) was
/// invisible and `switch_profile` ran against a ghost.
#[test]
fn drain_pending_switch_drops_a_target_deleted_after_enqueue() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta", Origin::Scheduler, now_ms() + 120_000);

    let mut disk = crate::profile::load_config().expect("load disk config");
    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from("beta"))
        .expect("rotation guard");
    crate::actions::delete_profile(
        &mut disk,
        &crate::profile::ProfileName::from("beta"),
        false,
        &guard,
    )
    .expect("delete");
    drop(guard);

    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("alpha"),
        "a deleted target must not be switched to"
    );
    assert!(
        queued_targets(&daemon).is_empty(),
        "a deleted target is dropped, not re-queued"
    );
    assert!(
        !crate::profile::profile_dir(&crate::profile::ProfileName::from("beta"))
            .expect("dir")
            .exists(),
        "the deleted target's directory must stay deleted"
    );
}

/// A switch's whole-state save must not re-list an unrelated profile deleted by
/// the CLI after the daemon's config was loaded. The switch still lands on the
/// (still-existing) target; only the deleted row stays gone.
#[test]
fn drain_pending_switch_does_not_resurrect_a_deleted_row() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("beta", "at-beta"),
            profile_with_creds("gamma", "at-gamma"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    stage_switch(&daemon, "beta", Origin::Scheduler, now_ms() + 120_000);

    let mut disk = crate::profile::load_config().expect("load disk config");
    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from("gamma"))
        .expect("rotation guard");
    crate::actions::delete_profile(
        &mut disk,
        &crate::profile::ProfileName::from("gamma"),
        false,
        &guard,
    )
    .expect("delete");
    drop(guard);

    daemon.drain_pending_switch();

    assert_eq!(
        active_of(&daemon).as_deref(),
        Some("beta"),
        "the switch to beta still lands"
    );
    let reloaded = crate::profile::load_config().expect("reload");
    assert!(
        reloaded
            .find(&crate::profile::ProfileName::from("gamma"))
            .is_none(),
        "the deleted profile's row must not come back through the switch's state save"
    );
}

/// The wrap-off's whole-state save is the same shape as the switch's: it must
/// not re-list a profile deleted out from under the daemon while it turns
/// everything off.
#[test]
fn drain_pending_switch_off_does_not_resurrect_a_deleted_row() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            profile_with_creds("alpha", "at-alpha"),
            profile_with_creds("gamma", "at-gamma"),
        ],
        Some("alpha"),
        90_000,
    );
    link_active_clean("alpha");
    let mut daemon = daemon_for(config);

    *daemon
        .pending_switch_off
        .lock()
        .expect("pending_switch_off") = true;

    let mut disk = crate::profile::load_config().expect("load disk config");
    let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from("gamma"))
        .expect("rotation guard");
    crate::actions::delete_profile(
        &mut disk,
        &crate::profile::ProfileName::from("gamma"),
        false,
        &guard,
    )
    .expect("delete");
    drop(guard);

    daemon.drain_pending_switch_off();

    assert_eq!(
        active_of(&daemon).as_deref(),
        None,
        "the wrap-off turned everything off"
    );
    let reloaded = crate::profile::load_config().expect("reload");
    assert!(
        reloaded
            .find(&crate::profile::ProfileName::from("gamma"))
            .is_none(),
        "the deleted profile's row must not come back through the switch-off state save"
    );
}

// ── CLAUTH_NO_API ───────────────────────────────────────────────────────────

/// The kill switch pinned at its CALL SITE, not just its predicate: with
/// `CLAUTH_NO_API=1` and a `--listen` address, `serve`'s listener decision must
/// yield the no-api arm — no certificate read, nothing for
/// `api::serve_prepared` to bind or import later. Deleting the `api_enabled()`
/// guard from the start path (leaving the predicate test green) re-arms a
/// listener the operator could only kill by editing the unit.
///
/// `Lego` (not a generated chain) and no `HomeSandbox` on purpose: under the
/// opt-out the decision never reads the certificate, so the arm is decided by
/// the env var alone. The pin's RED CHAIN is the certificate read: `prepare`
/// looks up this host's FQDN, then finds lego's directory through
/// `~/.clauth/tls.json` (which resolves `home_dir()`), so a regression that
/// deletes the guard dies at the sandbox panic — "test resolved the operator's
/// real home" — before any certificate file is opened, or at the `expect`
/// below when the FQDN lookup fails first. Both `assert`s never evaluate on
/// that edit; the panic is the red, and a legitimate one. A sandbox would
/// deadlock the guard another way: `HomeSandbox` holds `HOME_TEST_LOCK` for
/// the test's life and `with_no_api_env` takes it again.
#[test]
fn the_kill_switch_suppresses_the_listener_at_the_start_path() {
    with_no_api_env(Some("1"), || {
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().expect("addr");
        let (prepared, no_api) =
            super::listener_setup(Some(addr), &super::api::tls::CertSource::Lego)
                .expect("the opt-out is a decision, not a failure");
        assert!(
            prepared.is_none(),
            "CLAUTH_NO_API=1 must suppress the listener at the start path"
        );
        assert_eq!(
            no_api,
            Some(addr),
            "the opt-out still names the address it declined to serve"
        );
    });
}

/// `set_var`/`remove_var` are unsafe in Rust 2024 because they aren't
/// thread-safe in a multi-threaded process. Serialized here by `HOME_TEST_LOCK`
/// (the one mutex every env mutator across the suite takes) and undone before
/// the closure returns, so no other thread observes a torn value.
fn with_no_api_env<F: FnOnce()>(val: Option<&str>, f: F) {
    let _guard = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = std::env::var(super::NO_API_ENV).ok();
    // SAFETY: test-only, serialized by the lock above, restored unconditionally.
    unsafe {
        match val {
            Some(v) => std::env::set_var(super::NO_API_ENV, v),
            None => std::env::remove_var(super::NO_API_ENV),
        }
    }
    f();
    // SAFETY: same as above.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(super::NO_API_ENV, v),
            None => std::env::remove_var(super::NO_API_ENV),
        }
    }
}

/// `CLAUTH_NO_API=1` and nothing else disables the listener.
///
/// The exact-`"1"` rule matters more here than for its siblings: this is the
/// kill switch an operator reaches for when a listening socket has to go and the
/// unit passing `--listen` cannot be edited. A build that also honoured `"true"`
/// or `"0"` would silently drop the listener for someone who set it to `0`
/// meaning "off, don't disable" — and the symptom is a remote client going dark,
/// not an error anywhere.
#[test]
fn the_rest_api_is_disabled_only_by_exactly_one() {
    with_no_api_env(None, || {
        assert!(super::api_enabled(), "unset → the listener is available");
    });
    with_no_api_env(Some("1"), || {
        assert!(!super::api_enabled(), "CLAUTH_NO_API=1 → no listener");
    });
    for other in ["0", "true", "yes", "", "11", " 1"] {
        with_no_api_env(Some(other), || {
            assert!(
                super::api_enabled(),
                "CLAUTH_NO_API={other:?} is not the opt-out spelling"
            );
        });
    }
}

// ── publish_status: the switch-side republish ────────────────────────────────

/// The feed currently sitting in the sandbox, as a `Value`.
fn feed_on_disk() -> serde_json::Value {
    let path = clauth_dir().expect("clauth dir").join("status.json");
    serde_json::from_str(&std::fs::read_to_string(&path).expect("read status.json"))
        .expect("status.json is json")
}

/// Seed a feed the daemon could have written, naming `active` at `stamp`.
fn seed_feed(active: &str, stamp: &str) {
    let dir = clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("mkdir");
    std::fs::write(
        dir.join("status.json"),
        format!(r#"{{"schema":1,"generated_at":"{stamp}","active_profile":"{active}","pending_switch":null,"wrap_off":false,"refresh_interval_ms":120000,"profiles":[]}}"#),
    )
    .expect("seed status.json");
}

/// A switch landing outside the daemon republishes the feed, but keeps the
/// daemon's last `generated_at`: readers (`clauth-tray`, the TUI's daemon dot)
/// treat a fresh stamp as proof a daemon is alive, and stamping `now` from the
/// CLI would forge that proof with no daemon running.
#[test]
fn a_non_daemon_publish_preserves_the_daemons_last_stamp() {
    let _home = HomeSandbox::new();
    let stamp = "2026-09-01T00:00:00+00:00";
    seed_feed("alpha", stamp);
    let config = persist(
        vec![
            profile_with_creds("alpha", "a-1"),
            profile_with_creds("beta", "b-1"),
        ],
        Some("beta"),
        120_000,
    );

    through_handle(config, super::publish_status);

    let body = feed_on_disk();
    assert_eq!(
        body["generated_at"],
        serde_json::json!(stamp),
        "the stamp is the daemon's last write, not this publish's"
    );
    assert_eq!(body["active_profile"], serde_json::json!("beta"));
}

/// With nothing to preserve (no daemon has ever published here), the switch-side
/// publish stamps the epoch rather than `now`: the file still names the account
/// the operator switched to, while the staleness rule still reads "no daemon".
#[test]
fn a_non_daemon_publish_with_no_prior_feed_stamps_the_epoch() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![profile_with_creds("alpha", "a-1")],
        Some("alpha"),
        120_000,
    );

    through_handle(config, super::publish_status);

    let body = feed_on_disk();
    assert_eq!(
        body["generated_at"],
        serde_json::json!("1970-01-01T00:00:00+00:00")
    );
    assert_eq!(body["active_profile"], serde_json::json!("alpha"));
}

/// The daemon-owned form (the one the API's own switch republishes through)
/// stamps `now` even over an old file: a live daemon's write is itself the
/// freshness signal, so stamp preservation belongs to `publish_status` alone.
#[test]
fn a_direct_feed_write_stamps_now() {
    let _home = HomeSandbox::new();
    let stamp = "2026-09-01T00:00:00+00:00";
    seed_feed("alpha", stamp);
    let config = persist(
        vec![
            profile_with_creds("alpha", "a-1"),
            profile_with_creds("beta", "b-1"),
        ],
        Some("beta"),
        120_000,
    );

    super::write_status_feed(&config, None);

    let body = feed_on_disk();
    assert_ne!(
        body["generated_at"],
        serde_json::json!(stamp),
        "a daemon-side write is a freshness signal in itself"
    );
    assert_eq!(body["active_profile"], serde_json::json!("beta"));
}

#[test]
fn a_daemonless_publish_yields_to_a_feed_written_after_its_build_started() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            blank_profile(&crate::profile::ProfileName::from("a")),
            blank_profile(&crate::profile::ProfileName::from("b")),
        ],
        Some("b"),
        30_000,
    );

    let feed = clauth_dir().expect("feed dir").join("status.json");
    let incumbent: &[u8] = br#"{"sentinel": "incumbent"}"#;
    std::fs::write(&feed, incumbent).expect("seed incumbent feed");
    set_mtime(&feed, SystemTime::now() + Duration::from_secs(3600));

    let candidate: &[u8] = br#"{"active_profile": "b", "body": "candidate"}"#;
    super::publish_status_json_if_current(&config, candidate, SystemTime::now());
    assert_eq!(
        std::fs::read(&feed).expect("reread feed"),
        incumbent,
        "a feed written after the build started must survive the late commit"
    );

    set_mtime(&feed, SystemTime::now() - Duration::from_secs(3600));
    super::publish_status_json_if_current(&config, candidate, SystemTime::now());
    assert_eq!(
        std::fs::read(&feed).expect("reread feed"),
        candidate,
        "an older feed yields to the fresh body"
    );

    // The licensing rule is "stamped strictly before this build started": an
    // exactly-equal stamp must skip too (on a coarse-stamp filesystem an
    // equal stamp cannot prove the write preceded the build).
    let other: &[u8] = br#"{"sentinel": "second"}"#;
    std::fs::write(&feed, other).expect("reseed feed for the equality direction");
    let boundary = SystemTime::now();
    set_mtime(&feed, boundary);
    super::publish_status_json_if_current(&config, candidate, boundary);
    assert_eq!(
        std::fs::read(&feed).expect("reread feed"),
        other,
        "an exactly-equal stamp must skip, not publish"
    );
}

// ── a queued codex switch reaches the codex path (UPS-18) ───────────────────

/// Stage a switch on the CODEX slot, the way the socket enqueues one.
fn stage_codex_switch(d: &Daemon, target: &str, retry_until: u64) {
    d.pending_switch
        .lock()
        .expect("pending_switch")
        .push_back(PendingSwitchEntry {
            target: target.into(),
            origin: Origin::User,
            harness: crate::profile::Harness::Codex,
            retry_until,
        });
}

fn codex_roster_on_disk(active: &str, names: &[&str]) {
    let dir = clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&dir).expect("mkdir");
    let list = names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        dir.join("codex-profiles.toml"),
        format!("active_profile = \"{active}\"\nprofiles = [{list}]\n"),
    )
    .expect("write codex roster");
}

fn codex_active_on_disk() -> Option<String> {
    crate::codex_profiles::CodexState::load()
        .ok()?
        .active_profile()
        .map(|n| n.to_string())
}

/// The existence gate reads the roster the target's HARNESS names. Asking
/// `is_configured` (profiles.toml) about a codex target dropped every codex
/// switch as "profile no longer exists (deleted?)" — the socket answered `ok`
/// and the account never moved, which is worse than refusing the tap.
#[test]
fn a_queued_codex_switch_is_not_dropped_as_a_deleted_claude_profile() {
    let _home = HomeSandbox::new();
    codex_roster_on_disk("cx-a", &["cx-a", "cx-b"]);
    // A claude roster that has never heard of either codex name.
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![],
    };
    let mut daemon = daemon_for(config);

    stage_codex_switch(&daemon, "cx-b", now_ms() + 30_000);
    daemon.drain_pending_switch();

    assert_eq!(
        codex_active_on_disk().as_deref(),
        Some("cx-b"),
        "the codex slot must move; a claude-roster existence check drops it"
    );
    assert!(
        queued_targets(&daemon).is_empty(),
        "nothing re-queued after a switch that landed"
    );
}

/// The drop path still works for a codex name that really is gone — the gate
/// moved rosters, it did not stop guarding.
#[test]
fn a_codex_switch_to_a_name_the_roster_lost_is_still_dropped() {
    let _home = HomeSandbox::new();
    codex_roster_on_disk("cx-a", &["cx-a"]);
    let config = AppConfig {
        state: AppState::default(),
        profiles: vec![],
    };
    let mut daemon = daemon_for(config);

    stage_codex_switch(&daemon, "cx-gone", now_ms() + 30_000);
    daemon.drain_pending_switch();

    assert_eq!(codex_active_on_disk().as_deref(), Some("cx-a"), "unmoved");
    assert!(queued_targets(&daemon).is_empty(), "dropped, not retried");
}

// ── the headless half of the day-list collision warning ────────────────────

/// The daemon runs with nobody watching a toast, so the collision has to reach
/// the log — and reach it once. `day_claim_notices` holds the messages, so a
/// tick that re-derives the same state is silent and a claimant change is not.
///
/// The capture is what pins the EMISSION: the gate transitions below would
/// read identically if `logline!` were dropped from the loop.
#[test]
fn the_daemon_logs_a_day_collision_once_per_change() {
    use chrono::Weekday::*;
    let _home = crate::testutil::HomeSandbox::new();

    let all = || vec![Mon, Tue, Wed, Thu, Fri, Sat, Sun];
    let mut a = blank_profile(&crate::profile::ProfileName::from("work"));
    a.preferred_days = all();
    let mut b = blank_profile(&crate::profile::ProfileName::from("personal"));
    b.preferred_days = all();

    let mut config = persist(vec![a, b], Some("work"), 60_000);
    config.state.fallback_chain = config.state.profiles.clone();
    let mut daemon = daemon_for(config);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    daemon.log_day_claim_notices();
    let first = daemon
        .day_claim_notices
        .first()
        .cloned()
        .expect("two claimants raise a notice");
    assert!(first.contains("2 accounts claim"), "got {first}");
    assert_eq!(
        lines.snapshot().len(),
        1,
        "the notice reaches the log, not just the gate: {:?}",
        lines.snapshot()
    );
    assert!(
        lines.snapshot()[0].contains(&first),
        "the line carries the notice: {:?}",
        lines.snapshot()
    );

    daemon.log_day_claim_notices();
    assert_eq!(
        daemon.day_claim_notices,
        vec![first.clone()],
        "an unchanged tick leaves the gate where it was"
    );
    assert_eq!(
        lines.snapshot().len(),
        1,
        "and writes no second line: {:?}",
        lines.snapshot()
    );

    {
        let mut cfg = daemon.config.lock().expect("config mutex poisoned");
        if let Some(p) = cfg.find_mut(&crate::profile::ProfileName::from("personal")) {
            p.preferred_days.clear();
        }
    }
    daemon.log_day_claim_notices();
    assert!(
        daemon.day_claim_notices.is_empty(),
        "the gate clears so a collision re-introduced logs again"
    );
    assert_eq!(
        lines.snapshot().len(),
        1,
        "clearing a notice says nothing: {:?}",
        lines.snapshot()
    );
}

/// A failed attempt re-queued while a NEWER tap arrived keeps the newer tap
/// the winner: the retry goes to the front, and the winner is the last entry
/// of its origin. Pushed to the back, the stale retry landed and the newer tap,
/// already answered `ok` by the socket, was dropped (UPS-19 audit).
#[test]
fn a_requeued_retry_never_outranks_a_newer_tap() {
    let _home = HomeSandbox::new();
    let config = persist(
        vec![
            blank_profile(&"beta".into()),
            blank_profile(&"gamma".into()),
        ],
        Some("beta"),
        90_000,
    );
    let mut d = daemon_for(config);
    // The drain took "beta" out; the operator tapped "gamma" meanwhile.
    stage_switch(&d, "gamma", Origin::User, now_ms() + 60_000);
    d.requeue_quiet(PendingSwitchEntry {
        target: "beta".into(),
        origin: Origin::User,
        harness: crate::profile::Harness::Claude,
        retry_until: now_ms() + 60_000,
    });
    let queue = d.pending_switch.lock().expect("pending_switch").clone();
    let winner = crate::usage::select_switch_winner(&queue).expect("a winner");
    assert_eq!(
        winner.target.as_str(),
        "gamma",
        "the newer tap wins: {:?}",
        queued_targets(&d)
    );
}

/// The LaunchAgent must not run the daemon as a Background job: launchd then
/// pins it to the lowest CPU tier and throttles its disk I/O, and on a loaded
/// machine every tick step that touches a file took seconds (a 49s tick froze
/// status.json and false-failed a user switch, 2026-09-24).
#[test]
fn launch_agent_runs_the_daemon_at_standard_priority() {
    let plist = include_str!("../../dist/macos/com.clauth.daemon.plist");
    let key = plist
        .find("<key>ProcessType</key>")
        .expect("the plist names its process type");
    let value = &plist[key..];
    assert!(
        value
            .trim_start_matches("<key>ProcessType</key>")
            .trim_start()
            .starts_with("<string>Standard</string>"),
        "ProcessType must be Standard: {plist}"
    );
}
