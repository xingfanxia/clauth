mod actions;
mod claude;
mod completions;
mod fallback;
mod format;
mod lock;
mod oauth;
mod platform;
mod profile;
mod runtime;
mod start;
mod tui;
mod update;
mod ureq_error;
mod usage;
mod which;

use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::actions::{switch_profile, switch_profile_reconciled};
use crate::claude::{LinkState, classify_credentials_link, is_first_login};
use crate::profile::{AppConfig, load_config};
use crate::usage::{ActivityStore, RefetchQueue};

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
            // No scheduler running in the CLI path — queue contents are
            // discarded; we only need the return value (rotated names) which
            // the CLI also discards.
            let noop: RefetchQueue = Arc::new(Mutex::new(std::collections::HashSet::new()));
            // CLI path has no spinner — pass a throwaway ActivityStore so the
            // shared signature works without printing to stderr.
            let noop_activity: ActivityStore =
                Arc::new(Mutex::new(std::collections::HashMap::new()));
            let _ = oauth::refresh_all(&mut config, false, &noop, &noop_activity);

            // When the outgoing active profile has a diverged live credentials
            // file (CC re-logged or wrote a regular file), prompt rather than
            // refusing. On Yes: capture the live creds into the outgoing
            // profile first, then force the switch. On No: abort cleanly.
            let reconciled = if let Some(active) = config.state.active_profile.as_deref() {
                matches!(classify_credentials_link(active)?, LinkState::Diverged)
                    && !is_first_login(active)?
            } else {
                false
            };
            if reconciled {
                let active = config
                    .state
                    .active_profile
                    .as_deref()
                    .unwrap_or("")
                    .to_string();
                print!(
                    "active profile '{active}' has uncaptured credentials in ~/.claude \
                     (a re-login or token rotation). capture them into '{active}' and \
                     switch to '{canonical}'? [Y/n] "
                );
                use std::io::Write;
                std::io::stdout().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                let answer = answer.trim().to_ascii_lowercase();
                if answer.is_empty() || answer == "y" || answer == "yes" {
                    switch_profile_reconciled(&mut config, &canonical)?;
                } else {
                    println!("aborted — no changes made");
                    return Ok(());
                }
            } else {
                switch_profile(&mut config, &canonical)?;
            }

            // Match the TUI: prime the 5h window if the target is opted in
            // via `auto_start = true`. Cooldown blocks repeated CLI switches
            // from re-kicking inside the same window.
            let _ = oauth::auto_start_named(&mut config, &canonical, &noop, &noop_activity);
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
