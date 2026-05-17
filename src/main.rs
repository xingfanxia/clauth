mod actions;
mod claude;
mod completions;
mod menu;
mod platform;
mod profile;
mod ui;
mod update;
mod usage;

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};

use crate::actions::{capture_current_profile, create_blank_profile, is_cancelled, switch_profile};
use crate::claude::snapshot_active_credentials;
use crate::menu::{MainAction, MainMenuResult, build_main_menu, main_menu_prompt, profile_submenu};
use crate::profile::{Profile, load_config};
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

fn apply_usage(profiles: &mut [Profile], store: &usage::UsageStore) {
    let Ok(s) = store.lock() else {
        return;
    };
    for p in profiles {
        if let Some(info) = s.get(&p.name) {
            p.usage = Some(info.clone());
        }
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
    let _ = snapshot_active_credentials(&mut config);

    let usage_store: usage::UsageStore = Arc::new(Mutex::new(HashMap::new()));
    let usage_tokens: usage::TokenList = Arc::new(Mutex::new(collect_tokens(&config.profiles)));

    {
        let snapshot = usage_tokens.lock().unwrap().clone();
        usage::fetch_all_into(&snapshot, &usage_store);
    }
    usage::spawn_refresher(Arc::clone(&usage_tokens), Arc::clone(&usage_store));

    loop {
        apply_usage(&mut config.profiles, &usage_store);
        let menu = build_main_menu(&config);
        let labels: Vec<String> = menu.iter().map(|(l, _)| l.clone()).collect();

        let idx = match main_menu_prompt(labels, || {
            apply_usage(&mut config.profiles, &usage_store);
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

        *usage_tokens.lock().unwrap() = collect_tokens(&config.profiles);
    }

    Ok(())
}
