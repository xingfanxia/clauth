mod actions;
mod claude;
mod menu;
mod platform;
mod profile;
mod ui;
mod update;

use anyhow::Result;
use inquire::{InquireError, Select};

use crate::actions::{capture_current_profile, create_blank_profile, is_cancelled};
use crate::menu::{MainAction, build_main_menu, profile_submenu};
use crate::profile::load_config;
use crate::ui::build_render_config;

fn main() -> Result<()> {
    platform::init();
    update::spawn();
    inquire::set_global_render_config(build_render_config());
    let mut config = load_config()?;

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
