//! Showcase — a fake-data TUI for taking README screenshots. Compiled ONLY
//! under `#[cfg(test)]` (included via `#[path]` into `crate::tui`), so none of
//! this ships in the `clauth` binary and it lives outside `src/`.
//!
//! Launch it in a real terminal (it takes over the screen; press q twice to quit):
//!
//! ```text
//! cargo test showcase -- --ignored --nocapture
//! ```
//!
//! It builds a believable [`AppConfig`] from hard-coded demo values, redirects
//! the home dir at a throwaway tempdir, and runs the **real, fully-interactive**
//! TUI loop. Every action works for real — switch, edit, toggle, reorder, set
//! threshold, delete — but `home_dir()` is overridden so all reads/writes land
//! in the sandbox, never the user's real `~/.clauth` / `~/.claude`. The sandbox
//! tempdir is removed when it drops at the end of the run.
//!
//! `reconcile_startup` is deliberately never called, so `on_tick` never spawns
//! the bootstrap/scheduler (gated on `reconcile_done`) — no background worker,
//! no network. The demo profiles carry no credentials, so even the manual
//! refresh / rotate paths have no token to use and stay inert.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
};

use super::{TICK, Term, app, render, restore_terminal, setup_terminal};
use crate::profile::{AppConfig, AppState, Profile, ProfileName, home_dir, set_home_override};
use crate::usage::{
    ExtraUsage, FetchStatus, PlanInfo, ProfileActivity, UsageInfo, UsageWindow, now_ms,
};

// ── Launch ──────────────────────────────────────────────────────────────────

#[test]
#[ignore = "interactive TUI; run with `cargo test showcase -- --ignored --nocapture` in a real terminal"]
fn showcase() {
    run(demo_config()).expect("showcase loop");
}

/// Same as [`super::run`] but redirects home to a sandbox tempdir first.
fn run(config: AppConfig) -> Result<()> {
    let sandbox = tempfile::tempdir().context("create showcase sandbox dir")?;
    set_home_override(sandbox.path().to_path_buf());

    let mut terminal = setup_terminal()?;
    let outcome = showcase_loop(&mut terminal, config);
    let restore = restore_terminal(&mut terminal);
    outcome.and(restore) // `sandbox` drops here, cleaning up the tempdir
}

/// Real event loop without startup reconciliation — no bootstrap/scheduler spawns.
fn showcase_loop(terminal: &mut Term, config: AppConfig) -> Result<()> {
    let mut application = app::App::new(config);
    seed_usage(&application); // prime so windows show utilization, not `-`
    seed_timers(&application); // prime spinner and refresh countdowns
    let mut last_tick = Instant::now();

    while !application.quit {
        terminal.draw(|frame| render::draw(frame, &application))?;

        let timeout = TICK.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app::handle_key(&mut application, key);
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        if last_tick.elapsed() >= TICK {
            app::on_tick(&mut application);
            last_tick = Instant::now();
        }
    }

    Ok(())
}

/// RFC3339-ish string for `now + offset` (matches `iso_to_epoch_secs` format).
fn future_iso(offset: Duration) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + offset.as_secs();
    // no chrono dep; YYYY-MM-DDTHH:MM:SS+00:00 shape iso_to_epoch_secs parses
    let (y, mo, d, h, mi, sec) = epoch_to_parts(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{sec:02}+00:00")
}

fn epoch_to_parts(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Gregorian civil calendar — Howard Hinnant's algorithm, unsigned edition.
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, m, s)
}

#[allow(clippy::too_many_arguments)]
fn oauth_profile(
    name: &str,
    plan_type: &str,
    tier: &str,
    has_max: bool,
    has_pro: bool,
    auto_start: bool,
    fallback_threshold: Option<f64>,
    five_util: f64,
    five_resets_in: Option<Duration>,
    seven_sonnet: Option<(f64, Duration)>,
    seven_opus: Option<(f64, Duration)>,
    extra: Option<ExtraUsage>,
    fetch_status: Option<FetchStatus>,
) -> Profile {
    let five_hour = Some(UsageWindow {
        utilization: five_util,
        resets_at: five_resets_in.map(future_iso),
    });
    let seven_day_sonnet = seven_sonnet.map(|(u, reset)| UsageWindow {
        utilization: u,
        resets_at: Some(future_iso(reset)),
    });
    let seven_day_opus = seven_opus.map(|(u, reset)| UsageWindow {
        utilization: u,
        resets_at: Some(future_iso(reset)),
    });
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start,
        env: BTreeMap::new(),
        fallback_threshold,
        credentials: None,
        usage: Some(UsageInfo {
            plan: Some(PlanInfo {
                organization_type: Some(plan_type.to_string()),
                rate_limit_tier: Some(tier.to_string()),
                has_max,
                has_pro,
            }),
            five_hour,
            seven_day: None,
            seven_day_sonnet,
            seven_day_opus,
            extra_usage: extra,
        }),
        fetch_status,
    }
}

fn api_profile(name: &str) -> Profile {
    Profile {
        name: name.into(),
        base_url: Some("https://api.example.com".to_string()),
        api_key: Some(
            "sk-ant-api03-demo0000000000000000000000000000000000000000000000".to_string(),
        ),
        auto_start: false,
        env: BTreeMap::new(),
        fallback_threshold: None,
        credentials: None,
        usage: None,
        fetch_status: None,
    }
}

fn failed_profile(name: &str) -> Profile {
    Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        fallback_threshold: Some(90.0),
        credentials: None,
        usage: None,
        fetch_status: Some(FetchStatus::Failed),
    }
}

fn demo_config() -> AppConfig {
    let max20 = oauth_profile(
        "personal",
        "claude_max",
        "default_claude_max_20x",
        true,
        false,
        true,
        Some(80.0),
        64.3,
        Some(Duration::from_secs(2 * 3600 + 17 * 60)), // ~2h17m
        Some((22.1, Duration::from_secs(5 * 86400 + 6 * 3600))), // 7d sonnet ~5d
        Some((8.4, Duration::from_secs(6 * 86400 + 2 * 3600))), // 7d opus ~6d
        None,
        None,
    );

    let extra = ExtraUsage {
        is_enabled: true,
        monthly_limit: Some(100.00),
        used_credits: Some(42.50),
        utilization: Some(42.5),
        currency: Some("USD".to_string()),
    };
    let max5 = oauth_profile(
        "work",
        "claude_max",
        "default_claude_max_5x",
        true,
        false,
        true,
        Some(90.0),
        88.7,
        Some(Duration::from_secs(45 * 60)), // ~45m
        Some((61.2, Duration::from_secs(3 * 86400 + 9 * 3600))), // 7d sonnet ~3d
        Some((33.9, Duration::from_secs(6 * 86400 + 3600))), // 7d opus ~6d
        Some(extra),
        Some(FetchStatus::Cached), // cached → warning underline
    );

    let pro = oauth_profile(
        "side-project",
        "claude_pro",
        "default_claude_pro",
        false,
        true,
        false,
        Some(100.0),
        12.0,
        Some(Duration::from_secs(4 * 3600 + 5 * 60)),
        None,
        None,
        None,
        None,
    );

    let api = api_profile("bedrock-dev");

    let stale = failed_profile("research");

    let names: Vec<ProfileName> = [
        "personal",
        "work",
        "side-project",
        "bedrock-dev",
        "research",
    ]
    .iter()
    .map(|s| (*s).into())
    .collect();

    AppConfig {
        state: AppState {
            active_profile: Some("personal".into()),
            profiles: names,
            fallback_chain: vec!["personal".into(), "work".into(), "side-project".into()],
            ..AppState::default()
        },
        profiles: vec![max20, max5, pro, api, stale],
    }
}

/// Seed the live usage stores from demo profile data, as a real fetch worker would.
/// Without this, the first `on_tick` → `apply_usage` blanks all windows to `-`.
/// Reads config and drops the guard before touching usage locks (rank order:
/// `usage_store` / `usage_status` are both inner of `config`).
fn seed_usage(application: &app::App) {
    let snapshot: Vec<(String, Option<UsageInfo>, Option<FetchStatus>)> = {
        let cfg = application.config();
        cfg.profiles
            .iter()
            .map(|p| (p.name.to_string(), p.usage.clone(), p.fetch_status))
            .collect()
    };
    if let Ok(mut store) = application.usage_store.lock() {
        for (name, usage, _) in &snapshot {
            if let Some(u) = usage {
                store.insert(name.clone(), u.clone());
            }
        }
    }
    if let Ok(mut status) = application.usage_status.lock() {
        for (name, _, fetch_status) in &snapshot {
            if let Some(s) = fetch_status {
                status.insert(name.clone(), *s);
            }
        }
    }
}

/// Seed timer sources so the overview shows a spinner and countdowns (no
/// scheduler runs in the demo). Both stores are leaf-rank, in-memory only.
fn seed_timers(application: &app::App) {
    let now = now_ms();
    if let Ok(mut next) = application.next_refresh_per_profile.lock() {
        next.insert("work".to_string(), now + 43_000); // ~43s
        next.insert("side-project".to_string(), now + 78_000); // ~78s
    }
    if let Ok(mut activity) = application.activity.lock() {
        activity.insert("personal".to_string(), ProfileActivity::Fetching);
    }
}

#[test]
fn demo_config_has_expected_profiles() {
    let cfg = demo_config();
    assert_eq!(cfg.profiles.len(), 5);
    assert_eq!(cfg.state.active_profile.as_deref(), Some("personal"));
    assert_eq!(cfg.state.fallback_chain.len(), 3);

    let personal = cfg.profiles.iter().find(|p| p.name == "personal");
    assert!(personal.is_some_and(|p| p.auto_start && p.base_url.is_none()));

    let work = cfg.profiles.iter().find(|p| p.name == "work");
    assert!(work.is_some_and(|p| {
        p.fetch_status == Some(FetchStatus::Cached)
            && p.usage
                .as_ref()
                .and_then(|u| u.extra_usage.as_ref())
                .is_some_and(|e| e.is_enabled)
    }));

    let api = cfg.profiles.iter().find(|p| p.name == "bedrock-dev");
    assert!(api.is_some_and(|p| !p.is_oauth()));

    let failed = cfg.profiles.iter().find(|p| p.name == "research");
    assert!(
        failed.is_some_and(|p| p.fetch_status == Some(FetchStatus::Failed) && p.usage.is_none())
    );
}

#[test]
fn future_iso_parses() {
    use crate::usage::iso_to_epoch_secs;
    let s = future_iso(Duration::from_secs(3600));
    assert!(iso_to_epoch_secs(&s).is_some());
}

// ── Non-interactive driver ──────────────────────────────────────────────────
//
// Feeds synthetic key events through `handle_key` / `on_tick` so CI can prove
// every action — switch, edit, toggle, reorder, threshold, delete — without a
// TTY or touching the real `~/.clauth` / `~/.claude`.

/// Clears the home override on drop so it can't leak past this test on panic.
struct HomeOverrideReset;
impl Drop for HomeOverrideReset {
    fn drop(&mut self) {
        crate::profile::clear_home_override();
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn key_shift(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::SHIFT,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn press(app: &mut app::App, code: KeyCode) {
    app::handle_key(app, key(code));
}

/// Type a string one `Char` event at a time.
fn type_str(app: &mut app::App, s: &str) {
    for c in s.chars() {
        app::handle_key(app, key(KeyCode::Char(c)));
    }
}

/// Drain `on_tick` until `pred` holds or budget exhausted (async worker results).
fn settle(app: &mut app::App, what: &str, mut pred: impl FnMut(&app::App) -> bool) {
    for _ in 0..400 {
        app::on_tick(app);
        if pred(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("'{what}' never settled after draining ticks");
}

/// Helper: read profile fields without holding the config guard across handle_key.
fn base_url_of(app: &app::App, name: &str) -> Option<String> {
    app.config().find(name).and_then(|p| p.base_url.clone())
}

fn auto_start_of(app: &app::App, name: &str) -> bool {
    app.config()
        .find(name)
        .map(|p| p.auto_start)
        .unwrap_or(false)
}

fn threshold_of(app: &app::App, name: &str) -> Option<f64> {
    app.config().find(name).and_then(|p| p.fallback_threshold)
}

#[test]
fn demo_data_drives_all_actions() {
    // Hold home lock for the whole test; reset override on exit (even on panic).
    let _guard = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _reset = HomeOverrideReset;
    let sandbox = tempfile::tempdir().expect("create driver sandbox");
    set_home_override(sandbox.path().to_path_buf());

    assert_eq!(
        home_dir().expect("home dir"),
        sandbox.path(),
        "home override must redirect every FS access into the sandbox"
    );

    let mut app = app::App::new(demo_config());
    seed_usage(&app);
    seed_timers(&app);

    // reconcile_startup never called → no bootstrap, no scheduler.
    assert!(!app.reconcile_done && !app.bootstrap_started);

    // Pre-drive: timer slot has a spinner for the active profile and a countdown for an idle one.
    assert_eq!(
        app.activity.lock().unwrap().get("personal").copied(),
        Some(ProfileActivity::Fetching),
        "active profile must show a spinner in the timer slot"
    );
    assert!(
        app.next_refresh_per_profile
            .lock()
            .unwrap()
            .contains_key("work"),
        "idle profiles must show a refresh countdown in the timer slot"
    );

    // ── Tab navigation ──
    use app::Tab;
    assert_eq!(app.tab, Tab::Overview);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Usage);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Setup);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Fallback);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Config);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Overview, "→ wraps back to Overview");
    press(&mut app, KeyCode::Left);
    assert_eq!(app.tab, Tab::Config, "← wraps to the last tab");
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Left);
    assert_eq!(app.tab, Tab::Overview);

    // ── Switch ──
    assert_eq!(
        app.config().state.active_profile.as_deref(),
        Some("personal")
    );
    press(&mut app, KeyCode::Down); // 0 (personal) → 1 (work)
    press(&mut app, KeyCode::Enter); // request switch → confirm modal
    assert_eq!(app.modals.len(), 1, "switch raises a confirm modal");
    press(&mut app, KeyCode::Enter); // accept (choice defaults to yes)
    assert!(app.modals.is_empty(), "confirming pops the modal");
    settle(&mut app, "switch to work", |a| {
        a.config().state.active_profile.as_deref() == Some("work")
    });

    // Seeded usage windows must survive on_tick → apply_usage during the switch.
    {
        let cfg = app.config();
        let util = cfg
            .find("personal")
            .and_then(|p| p.usage.as_ref())
            .and_then(|u| u.five_hour.as_ref())
            .map(|w| w.utilization);
        assert_eq!(
            util,
            Some(64.3),
            "seeded 5h utilization must survive on_tick → apply_usage"
        );
    }

    let state_file = sandbox.path().join(".clauth").join("profiles.toml"); // switch persists to sandbox, not real config
    assert!(
        state_file.exists(),
        "switch must write profiles.toml inside the sandbox"
    );

    // ── Edit ── (cursor at "work"/1 after switch; one ↓ → "side-project"/2)
    press(&mut app, KeyCode::Right); // Overview → Usage
    press(&mut app, KeyCode::Right); // Usage → Setup
    assert_eq!(app.tab, Tab::Setup);
    assert_eq!(app.profile_cursor, 1, "cursor carried over from the switch");
    press(&mut app, KeyCode::Down); // 1 → 2 (side-project)
    press(&mut app, KeyCode::Enter); // focus the detail pane
    assert_eq!(app.config_focus, app::ConfigFocus::Actions);
    assert!(app.config_draft.is_some());
    press(&mut app, KeyCode::Down); // Name → BaseUrl
    press(&mut app, KeyCode::Enter); // start capturing the field
    assert_eq!(
        app.config_draft.as_ref().and_then(|d| d.active),
        Some(app::ConfigRow::BaseUrl)
    );
    type_str(&mut app, "https://proxy.test");
    press(&mut app, KeyCode::Enter); // commit the field
    assert_eq!(
        base_url_of(&app, "side-project").as_deref(),
        Some("https://proxy.test"),
        "editing the BaseUrl field must persist it"
    );
    press(&mut app, KeyCode::Esc); // back out of the detail pane
    assert_eq!(app.config_focus, app::ConfigFocus::Profiles);

    // ── Toggle ──
    assert!(auto_start_of(&app, "personal"), "demo seeds personal ON");
    press(&mut app, KeyCode::Up); // 2 → 1 (work)
    press(&mut app, KeyCode::Up); // → 0 (personal)
    press(&mut app, KeyCode::Enter); // focus detail for personal
    press(&mut app, KeyCode::Down); // Name → BaseUrl
    press(&mut app, KeyCode::Down); // BaseUrl → ApiKey
    press(&mut app, KeyCode::Down); // ApiKey → AutoStart
    press(&mut app, KeyCode::Enter); // flip it
    assert!(
        !auto_start_of(&app, "personal"),
        "auto-start must toggle off"
    );
    press(&mut app, KeyCode::Esc);

    // ── Reorder ──
    press(&mut app, KeyCode::Right); // Config → Fallback
    assert_eq!(app.tab, Tab::Fallback);
    {
        let cfg = app.config();
        assert_eq!(
            cfg.state.fallback_chain,
            vec!["personal", "work", "side-project"]
        );
    }
    app::handle_key(&mut app, key_shift(KeyCode::Down)); // head → one step down
    {
        let cfg = app.config();
        assert_eq!(
            cfg.state.fallback_chain,
            vec!["work", "personal", "side-project"],
            "⇧↓ reorders the chain"
        );
    }
    assert_eq!(app.chain_cursor, 1, "cursor follows the moved member");

    // ── Set threshold (stepper) ── "personal" is chain index 1, 80% → 85%
    assert_eq!(threshold_of(&app, "personal"), Some(80.0));
    press(&mut app, KeyCode::Enter); // enter member detail pane
    assert_eq!(app.fallback_focus, app::FallbackFocus::Detail);
    press(&mut app, KeyCode::Char('+')); // +5 step
    assert_eq!(
        threshold_of(&app, "personal"),
        Some(85.0),
        "the + stepper bumps the threshold by 5"
    );

    // ── Set threshold (inline editor) ──
    press(&mut app, KeyCode::Enter); // open inline editor on Threshold row
    assert!(app.fallback_threshold_draft.is_some());
    press(&mut app, KeyCode::Backspace); // clear "85" (2 chars)
    press(&mut app, KeyCode::Backspace);
    type_str(&mut app, "50");
    press(&mut app, KeyCode::Enter); // commit
    assert!(app.fallback_threshold_draft.is_none());
    assert_eq!(
        threshold_of(&app, "personal"),
        Some(50.0),
        "the inline editor sets an absolute threshold"
    );
    press(&mut app, KeyCode::Esc); // leave the detail pane
    assert_eq!(app.fallback_focus, app::FallbackFocus::Chain);

    // ── Delete ──
    let before = app.profile_count();
    press(&mut app, KeyCode::Left); // Fallback → Setup
    assert_eq!(app.tab, Tab::Setup);
    for _ in 0..4 {
        press(&mut app, KeyCode::Down); // 0 → 4 ("research")
    }
    press(&mut app, KeyCode::Enter); // focus detail
    for _ in 0..4 {
        press(&mut app, KeyCode::Down); // Name → Delete (last row)
    }
    press(&mut app, KeyCode::Enter); // arm
    assert!(
        app.config_draft
            .as_ref()
            .map(|d| d.armed_delete)
            .unwrap_or(false),
        "first ⏎ arms the delete row"
    );
    press(&mut app, KeyCode::Enter); // confirm
    assert_eq!(app.profile_count(), before - 1, "delete drops one profile");
    assert!(
        app.config().find("research").is_none(),
        "the deleted profile is gone from the config"
    );

    press(&mut app, KeyCode::Char('q'));
    assert!(app.armed_quit, "first q arms the quit");
    assert!(!app.quit, "first q does not quit yet");
    assert!(app.footer_alert.is_some(), "first q sets a footer alert");

    // any non-q key disarms the alert (Esc at top level is a no-op but still disarms)
    press(&mut app, KeyCode::Esc);
    assert!(!app.armed_quit, "unhandled key disarms quit");
    assert!(app.footer_alert.is_none(), "disarm clears footer alert");

    // x dismisses the alert directly when no toasts are queued
    app.toasts.clear(); // drain any toasts from earlier actions
    press(&mut app, KeyCode::Char('q'));
    assert!(app.footer_alert.is_some(), "re-arm sets alert");
    press(&mut app, KeyCode::Char('x'));
    assert!(app.footer_alert.is_none(), "x dismisses the footer alert");
    assert!(!app.armed_quit, "x also disarms quit");

    press(&mut app, KeyCode::Char('q'));
    press(&mut app, KeyCode::Char('q'));
    assert!(app.quit, "second q confirms quit");

    // Nothing escaped the sandbox — home_dir still points there.
    assert_eq!(home_dir().expect("home dir"), sandbox.path());
}
