mod actions;
mod claude;
mod completions;
mod fallback;
mod lock;
mod oauth;
mod platform;
mod profile;
mod tui;
mod update;
mod ureq_error;
mod usage;

use anyhow::Result;

use crate::actions::switch_profile;
use crate::profile::load_config;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.as_slice() {
        [cmd, sub] if cmd == "completions" && sub == "install" => {
            return completions::install(None);
        }
        [cmd, sub, shell] if cmd == "completions" && sub == "install" => {
            return completions::install(Some(shell));
        }
        [cmd, shell] if cmd == "completions" => return completions::print_script(shell),
        [cmd] if cmd == "__complete" => {
            completions::print_profile_names();
            return Ok(());
        }
        [cmd] if cmd == "--help" || cmd == "-h" => {
            print_help();
            return Ok(());
        }
        [cmd] if cmd == "--version" || cmd == "-V" => {
            println!("clauth {}", env!("CARGO_PKG_VERSION"));
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
            // Refresh every profile's OAuth token before switching, same as
            // the interactive flow. The rotated access token then ends up in
            // ~/.claude/.credentials.json via the symlink.
            let _ = oauth::refresh_all(&mut config);
            switch_profile(&mut config, &canonical)?;
            println!("switched to '{canonical}'");
            return Ok(());
        }
        [] => {}
        _ => anyhow::bail!(
            "usage: clauth [profile] | clauth completions <bash|zsh|fish> | clauth completions install [shell]"
        ),
    }

    platform::init();
    completions::auto_install_once();
    update::spawn();
    let config = load_config()?;
    tui::run(config)
}

fn print_help() {
    println!(
        "clauth {ver} — Claude Code account switcher\n\n\
         Usage:\n  \
           clauth                          launch the TUI\n  \
           clauth <profile>                switch to profile by name and exit\n  \
           clauth completions <shell>      print shell completion script (bash|zsh|fish)\n  \
           clauth completions install [shell]\n                                  \
         install completions into the user's shell rc\n  \
           clauth --version                print version\n  \
           clauth --help                   show this help",
        ver = env!("CARGO_PKG_VERSION"),
    );
}
