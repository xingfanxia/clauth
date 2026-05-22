mod actions;
mod claude;
mod completions;
mod fallback;
mod format;
mod lock;
mod oauth;
mod platform;
mod profile;
mod start;
mod tui;
mod update;
mod ureq_error;
mod usage;
mod which;

use anyhow::Result;

use crate::actions::switch_profile;
use crate::profile::{AppConfig, load_config};

fn resolve_or_bail(config: &AppConfig, name: &str) -> Result<String> {
    config.canonical_name(name).ok_or_else(|| {
        let available = config.names().join(", ");
        anyhow::anyhow!("profile '{name}' not found\navailable: {available}")
    })
}

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
        [cmd] if cmd == "which" => return which::run(false),
        [cmd, flag] if cmd == "which" && flag == "--json" => return which::run(true),
        [cmd, _, ..] if cmd == "which" => {
            anyhow::bail!("usage: clauth which [--json]");
        }
        [cmd] if cmd == "start" => {
            anyhow::bail!("usage: clauth start <profile> [claude args...]");
        }
        [cmd, name, rest @ ..] if cmd == "start" => {
            platform::init();
            let config = load_config()?;
            let canonical = resolve_or_bail(&config, name)?;
            return start::run(&config, &canonical, rest);
        }
        [name] => {
            platform::init();
            let mut config = load_config()?;
            let canonical = resolve_or_bail(&config, name)?;
            // Refresh every profile's OAuth token before switching, same as
            // the interactive flow. The rotated access token then ends up in
            // ~/.claude/.credentials.json via the symlink.
            let _ = oauth::refresh_all(&mut config);
            switch_profile(&mut config, &canonical)?;
            // Match the TUI: prime the 5h window if the target is opted in
            // via `auto_start = true`. Cooldown blocks repeated CLI switches
            // from re-kicking inside the same window.
            let _ = oauth::auto_start_named(&mut config, &canonical);
            println!("switched to '{canonical}'");
            return Ok(());
        }
        [] => {}
        _ => anyhow::bail!(
            "usage: clauth [profile] | clauth start <profile> [claude args...] | clauth which [--json] | clauth completions <bash|zsh|fish> | clauth completions install [shell]"
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
           clauth start <profile> [args]   launch claude with that profile's settings\n                                  \
         in an isolated CLAUDE_CONFIG_DIR; extra args go to claude\n  \
           clauth which [--json]           print the profile owning the loaded\n                                  \
         credentials.json (CLAUDE_CONFIG_DIR-aware); `unknown` on no match\n  \
           clauth completions <shell>      print shell completion script (bash|zsh|fish)\n  \
           clauth completions install [shell]\n                                  \
         install completions into the user's shell rc\n  \
           clauth --version                print version\n  \
           clauth --help                   show this help",
        ver = env!("CARGO_PKG_VERSION"),
    );
}
