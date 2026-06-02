//! Showcase — a fake-data TUI for taking README screenshots. Compiled ONLY
//! under `#[cfg(test)]` (included via `#[path]` into `crate::tui`), so none of
//! this ships in the `clauth` binary and it lives outside `src/`.
//!
//! Launch it in a real terminal (it takes over the screen; press q / ⎋ to quit):
//!
//! ```text
//! cargo test showcase -- --ignored --nocapture
//! ```
//!
//! It builds a believable [`AppConfig`] from hard-coded demo values and runs a
//! stripped-down event loop that skips `reconcile_startup` / `on_tick` /
//! `shutdown` — no network calls, no filesystem writes, no real config touched.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use super::{TICK, Term, app, render, restore_terminal, setup_terminal};
use crate::profile::{AppConfig, AppState, Profile};
use crate::usage::{ExtraUsage, FetchStatus, PlanInfo, UsageInfo, UsageWindow};

// ── Launch ──────────────────────────────────────────────────────────────────

#[test]
#[ignore = "interactive TUI; run with `cargo test showcase -- --ignored --nocapture` in a real terminal"]
fn showcase() {
    run(demo_config()).expect("showcase loop");
}

/// Same terminal setup/teardown as [`super::run`], but a stripped-down loop:
/// draw + handle keys, no startup/worker paths.
fn run(config: AppConfig) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let outcome = showcase_loop(&mut terminal, config);
    let restore = restore_terminal(&mut terminal);
    outcome.and(restore)
}

fn showcase_loop(terminal: &mut Term, config: AppConfig) -> Result<()> {
    let mut application = app::App::new(config);
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
            // Advance tick_count so blink/spinner animations still work.
            application.tick_count = application.tick_count.wrapping_add(1);
            last_tick = Instant::now();
        }
    }

    Ok(())
}

// ── Time helper ───────────────────────────────────────────────────────────────

/// Returns an RFC3339-ish string (matching `iso_to_epoch_secs` expectations)
/// for `now + offset`.
fn future_iso(offset: Duration) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + offset.as_secs();
    // Manual RFC3339 formatting — no chrono dep needed; matches the
    // `YYYY-MM-DDTHH:MM:SS+00:00` shape that `iso_to_epoch_secs` parses.
    let (y, mo, d, h, mi, sec) = epoch_to_parts(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{sec:02}+00:00")
}

fn epoch_to_parts(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Gregorian civil calendar (Howard Hinnant's algorithm, unsigned edition).
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

// ── Profile builders ──────────────────────────────────────────────────────────

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
    seven_sonnet: Option<f64>,
    seven_opus: Option<f64>,
    extra: Option<ExtraUsage>,
    fetch_status: Option<FetchStatus>,
) -> Profile {
    let five_hour = Some(UsageWindow {
        utilization: five_util,
        resets_at: five_resets_in.map(future_iso),
    });
    let seven_day_sonnet = seven_sonnet.map(|u| UsageWindow {
        utilization: u,
        resets_at: None,
    });
    let seven_day_opus = seven_opus.map(|u| UsageWindow {
        utilization: u,
        resets_at: None,
    });
    Profile {
        name: name.to_string(),
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
        name: name.to_string(),
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
        name: name.to_string(),
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

// ── Demo config ───────────────────────────────────────────────────────────────

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
        Some(Duration::from_secs(2 * 3600 + 17 * 60)), // resets in ~2h17m
        Some(22.1),
        Some(8.4),
        None,
        None, // live / fresh, no underline
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
        Some(Duration::from_secs(45 * 60)), // resets in ~45m
        Some(61.2),
        Some(33.9),
        Some(extra),
        Some(FetchStatus::Cached), // warning underline
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

    let names: Vec<String> = [
        "personal",
        "work",
        "side-project",
        "bedrock-dev",
        "research",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    AppConfig {
        state: AppState {
            active_profile: Some("personal".to_string()),
            profiles: names,
            fallback_chain: vec![
                "personal".to_string(),
                "work".to_string(),
                "side-project".to_string(),
            ],
            ..AppState::default()
        },
        profiles: vec![max20, max5, pro, api, stale],
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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
