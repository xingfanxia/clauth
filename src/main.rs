mod actions;
mod claude;
mod completions;
mod fallback;
mod lock;
mod menu;
mod oauth;
mod platform;
mod profile;
mod ui;
mod update;
mod ureq_error;
mod usage;

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::Result;
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};
use inquire::{Confirm, InquireError};

use crate::actions::{capture_current_profile, create_blank_profile, is_cancelled, switch_profile};
use crate::claude::{
    credentials_diverged, detach_credentials_link, link_profile_credentials,
    read_claude_credentials, snapshot_active_credentials,
};
use crate::fallback::auto_switch_if_needed;
use crate::menu::{
    MainAction, MainMenuResult, build_main_menu, fallback_chain_menu, main_menu_prompt,
    profile_submenu,
};
use crate::profile::{AppConfig, Profile, app_state_mtime, load_config, save_app_state};
use crate::ui::build_render_config;

fn collect_tokens(profiles: &[Profile]) -> Vec<(String, String)> {
    profiles
        .iter()
        .filter_map(|p| {
            let token = p
                .credentials
                .as_ref()?
                .claude_ai_oauth
                .as_ref()?
                .access_token
                .clone();
            Some((p.name.clone(), token))
        })
        .collect()
}

fn apply_usage(profiles: &mut [Profile], store: &usage::UsageStore, status: &usage::StatusStore) {
    let info_map = store.lock().ok();
    let status_map = status.lock().ok();
    for p in profiles {
        if let Some(s) = info_map.as_ref()
            && let Some(info) = s.get(&p.name)
        {
            p.usage = Some(info.clone());
        }
        p.fetch_status = status_map.as_ref().and_then(|s| s.get(&p.name).copied());
    }
}

/// Resolve the gap between the live `~/.claude/.credentials.json` and the
/// active profile's stored credentials at startup. The normal case is a
/// silent snapshot — same tokens, or just a refresh that rotated them. The
/// interesting case is when both access_token and/or refresh_token differ
/// from what we have stored: that usually means Claude Code was used to
/// sign into a different account while clauth wasn't running, and a blind
/// snapshot would overwrite the active profile's identity. Ask first.
fn reconcile_startup_credentials(config: &mut AppConfig) -> Result<()> {
    let Some(active) = config.state.active_profile.clone() else {
        return Ok(());
    };

    let live = read_claude_credentials().ok().flatten();
    let stored = config.find(&active).and_then(|p| p.credentials.as_ref());

    if !credentials_diverged(stored, live.as_ref()) {
        let _ = snapshot_active_credentials(config);
        return Ok(());
    }

    let keep = match Confirm::new(&format!("Still logged in as '{active}'?"))
        .with_default(true)
        .with_help_message("~/.claude/.credentials.json differs from this profile's saved tokens.")
        .prompt()
    {
        Ok(b) => b,
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => true,
        Err(e) => return Err(e.into()),
    };

    if keep {
        let _ = snapshot_active_credentials(config);
        return Ok(());
    }

    detach_credentials_link()?;
    config.state.active_profile = None;
    save_app_state(&config.state)?;

    let capture = match Confirm::new("Capture current credentials as a new profile?")
        .with_default(true)
        .prompt()
    {
        Ok(b) => b,
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => false,
        Err(e) => return Err(e.into()),
    };

    if capture
        && let Err(e) = capture_current_profile(config)
        && !is_cancelled(&e)
    {
        return Err(e);
    }

    Ok(())
}

/// Pick up edits from a concurrent clauth instance (or hand edits in
/// ~/.clauth/) by reloading the on-disk config when profiles.toml has been
/// rewritten since we last looked. Returns true if a reload happened so the
/// caller can refresh derived state like the usage token list.
fn reload_if_state_changed(config: &mut AppConfig, last_seen: &mut Option<SystemTime>) -> bool {
    let current = app_state_mtime();
    if current == *last_seen {
        return false;
    }
    match load_config() {
        Ok(fresh) => {
            *config = fresh;
            *last_seen = current;
            true
        }
        Err(_) => false,
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.as_slice() {
        [cmd, shell] if cmd == "completions" => return completions::print_script(shell),
        [cmd] if cmd == "__complete" => {
            completions::print_profile_names();
            return Ok(());
        }
        [name] => {
            platform::init();
            let mut config = load_config()?;
            let canonical = config
                .names()
                .into_iter()
                .find(|n| n.eq_ignore_ascii_case(name))
                .map(str::to_string);
            let Some(canonical) = canonical else {
                let available = config.names().join(", ");
                anyhow::bail!("profile '{name}' not found\navailable: {available}");
            };
            switch_profile(&mut config, &canonical)?;
            println!("switched to '{canonical}'");
            return Ok(());
        }
        [] => {}
        _ => anyhow::bail!("usage: clauth [profile] | clauth completions <bash|zsh|fish>"),
    }

    platform::init();
    completions::auto_install_once();
    update::spawn();
    inquire::set_global_render_config(build_render_config());
    let mut config = load_config()?;
    reconcile_startup_credentials(&mut config)?;
    // Re-establish the symlink so in-session Claude Code refreshes flow
    // back into the active profile's storage. The previous clauth exit
    // replaced the symlink with a plain copy so external writes between
    // sessions wouldn't bleed into the wrong profile.
    if let Some(active) = config.state.active_profile.clone() {
        let _ = link_profile_credentials(&active);
    }

    let usage_store: usage::UsageStore = Arc::new(Mutex::new(HashMap::new()));
    let usage_status: usage::StatusStore = Arc::new(Mutex::new(HashMap::new()));
    let usage_tokens: usage::TokenList = Arc::new(Mutex::new(collect_tokens(&config.profiles)));

    // Initial usage fetch: tells us which profiles already have a running
    // 5-hour window vs. which need their token refreshed to kick one off
    // (Claude Code does the same thing on launch).
    {
        let snapshot = usage_tokens
            .lock()
            .expect("usage_tokens mutex poisoned")
            .clone();
        usage::fetch_all_into(&snapshot, &usage_store, &usage_status);
    }

    // For profiles that opted in with `kick_timer = true` in their
    // config.toml and have no running 5-hour window, refresh + fire a
    // 1-token Haiku ping to start the window, then re-fetch usage so the
    // menu confirms the timer started.
    let kicked = oauth::kick_missing_timers(&mut config, &usage_store);
    if !kicked.is_empty() {
        let retry: Vec<(String, String)> = collect_tokens(&config.profiles)
            .into_iter()
            .filter(|(name, _)| kicked.contains(name))
            .collect();
        usage::fetch_all_into(&retry, &usage_store, &usage_status);
        *usage_tokens.lock().expect("usage_tokens mutex poisoned") =
            collect_tokens(&config.profiles);
    }
    usage::spawn_refresher(
        Arc::clone(&usage_tokens),
        Arc::clone(&usage_store),
        Arc::clone(&usage_status),
    );

    apply_usage(&mut config.profiles, &usage_store, &usage_status);
    let _ = auto_switch_if_needed(&mut config);

    let mut last_state_mtime = app_state_mtime();

    loop {
        apply_usage(&mut config.profiles, &usage_store, &usage_status);
        let menu = build_main_menu(&config);
        let labels: Vec<String> = menu.iter().map(|(l, _)| l.clone()).collect();

        let idx = match main_menu_prompt(labels, || {
            // Pick up state edits from a concurrent instance (or hand edits).
            // Done before apply_usage so the fresh profile list gets its
            // usage filled in from our cache in the same tick.
            if reload_if_state_changed(&mut config, &mut last_state_mtime) {
                *usage_tokens.lock().expect("usage_tokens mutex poisoned") =
                    collect_tokens(&config.profiles);
            }
            apply_usage(&mut config.profiles, &usage_store, &usage_status);
            let _ = auto_switch_if_needed(&mut config);
            build_main_menu(&config)
                .into_iter()
                .map(|(label, _)| label)
                .collect()
        })? {
            MainMenuResult::Selected(idx) => idx,
            MainMenuResult::Cancelled => break,
        };

        // After main_menu_prompt drops its RawModeGuard the cursor sits where
        // the menu started; remember it so we can wipe any inquire artifacts
        // the next action leaves behind before re-rendering.
        let menu_row = crossterm::cursor::position().map(|(_, r)| r).ok();

        let result = match menu[idx].1 {
            MainAction::Quit => break,
            MainAction::NewBlank => create_blank_profile(&mut config),
            MainAction::Capture => capture_current_profile(&mut config),
            MainAction::FallbackChain => fallback_chain_menu(&mut config),
            MainAction::Profile(i) => {
                let name = config.profiles[i].name.clone();
                profile_submenu(&mut config, &name)
            }
        };

        if let Err(e) = result
            && !is_cancelled(&e)
        {
            return Err(e);
        }

        if let Some(row) = menu_row {
            let _ = execute!(
                io::stdout(),
                MoveTo(0, row),
                Clear(ClearType::FromCursorDown)
            );
        }

        *usage_tokens.lock().expect("usage_tokens mutex poisoned") =
            collect_tokens(&config.profiles);
    }

    // Persist whatever Claude Code wrote during this session, then replace
    // the symlink with a plain copy of its current target. After shutdown
    // any external write to ~/.claude/.credentials.json (a different
    // account sign-in, for example) lands in that standalone file instead
    // of mutating the active profile's storage through the link — so the
    // next startup can detect the divergence and ask the user.
    let _ = snapshot_active_credentials(&mut config);
    let _ = detach_credentials_link();

    Ok(())
}
