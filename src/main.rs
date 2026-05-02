mod actions;
mod claude;
mod menu;
mod platform;
mod profile;
mod ui;
mod update;
mod usage;

use anyhow::Result;
use inquire::{InquireError, Select};

use crate::actions::{capture_current_profile, create_blank_profile, is_cancelled, switch_profile};
use crate::claude::snapshot_active_credentials;
use crate::menu::{MainAction, build_main_menu, profile_submenu};
use crate::profile::load_config;
use crate::ui::build_render_config;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if let [name] = args.as_slice() {
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

    if args.len() > 1 {
        anyhow::bail!("usage: clauth [profile]");
    }

    platform::init();
    update::spawn();
    inquire::set_global_render_config(build_render_config());
    let mut config = load_config()?;
    let _ = snapshot_active_credentials(&mut config);

    let usage_handles: Vec<(usize, std::thread::JoinHandle<Option<usage::UsageInfo>>)> = config
        .profiles
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let name = p.name.clone();
            let token = p
                .credentials
                .as_ref()?
                .claude_ai_oauth
                .as_ref()?
                .access_token
                .clone();
            Some((
                i,
                std::thread::spawn(move || usage::fetch_cached(&name, &token)),
            ))
        })
        .collect();

    for (i, handle) in usage_handles {
        config.profiles[i].usage = handle.join().ok().flatten();
    }

    loop {
        let menu = build_main_menu(&config);
        let labels: Vec<String> = menu.iter().map(|(l, _)| l.clone()).collect();

        let idx = match Select::new("clauth", labels)
            .without_filtering()
            .raw_prompt()
        {
            Ok(opt) => opt.index,
            Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => break,
            Err(e) => return Err(e.into()),
        };

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
    }

    Ok(())
}
