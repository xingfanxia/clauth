mod actions;
mod claude;
mod claude_json;
mod completions;
mod fallback;
mod format;
mod lock;
mod lockorder;
mod oauth;
mod platform;
mod profile;
mod runtime;
mod spinner;
mod start;
mod tui;
mod update;
mod ureq_error;
mod usage;
mod which;

use std::sync::Arc;

use anyhow::Result;

use crate::actions::{switch_profile, switch_profile_reconciled};
use crate::claude::{LinkState, classify_credentials_link, is_first_login};
use crate::lockorder::RankedMutex;
use crate::profile::{AppConfig, load_config};
use crate::spinner::Spinner;
use crate::usage::{ActivityStore, OpResult, RefetchQueue};

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
            let config = load_config()?;
            let canonical = resolve_or_bail(&config, name)?;
            // Rotate only the outgoing active and incoming target profiles
            // before the FS relink. Rotating every other profile's single-use
            // refresh token on every switch is unnecessary and widens races with
            // the scheduler.
            let outgoing = config.state.active_profile.clone();
            // No scheduler running — noop_refetch is a throwaway; auto_start_named
            // below still uses it to push kicked names no one reads.
            let noop_refetch: RefetchQueue =
                Arc::new(RankedMutex::new(std::collections::HashSet::new()));
            // CLI path has no spinner — pass a throwaway ActivityStore so the
            // shared signature works without printing to stderr.
            let noop_activity: ActivityStore =
                Arc::new(RankedMutex::new(std::collections::HashMap::new()));
            // CLI has no OpResult drain — drop the receiver immediately so
            // workers' `sender.send` returns disconnected-error which they
            // ignore (`let _ = …`). The Arc<Mutex<AppConfig>> wraps the
            // owned config so oauth fns can take/drop the lock per their
            // contract.
            let (op_sender, _op_receiver) = std::sync::mpsc::channel::<OpResult>();

            // Classify the outgoing active profile's live link BEFORE any
            // rotation. A diverged link means CC re-logged or rotated and wrote
            // a regular file — a different, still-valid chain. Rotating the
            // STORED chain in that case burns a single-use refresh token that
            // the reconcile path (`force_snapshot_active_credentials`) then
            // discards when it captures the live creds. Computing the verdict
            // first lets us skip the doomed rotation. These checks are pure
            // path/FS reads (no network, no config lock).
            let reconciled = match outgoing.as_deref() {
                Some(active) => {
                    matches!(classify_credentials_link(active)?, LinkState::Diverged)
                        && !is_first_login(active)?
                }
                None => false,
            };

            let config = Arc::new(RankedMutex::new(config));
            {
                // Scoped so the spinner stops before the interactive [Y/n]
                // prompt below — a live spinner during stdin read corrupts it.
                let _spinner = Spinner::start("clauth: rotating tokens…");
                // Skip the outgoing rotation when its live link diverged: its
                // stored chain is about to be overwritten by the live creds, so
                // rotating it only burns a refresh token for nothing.
                if let Some(ref active) = outgoing
                    && active != &canonical
                    && !reconciled
                {
                    oauth::rotate_one(&config, active, &noop_activity, &op_sender);
                }
                oauth::rotate_one(&config, &canonical, &noop_activity, &op_sender);
            }

            // When the outgoing active profile has a diverged live credentials
            // file (CC re-logged or wrote a regular file), prompt rather than
            // refusing. On Yes: capture the live creds into the outgoing
            // profile first, then force the switch. On No: abort cleanly.
            if reconciled {
                let active = {
                    let cfg = config.lock().expect("config mutex poisoned");
                    cfg.state
                        .active_profile
                        .as_deref()
                        .unwrap_or("")
                        .to_string()
                };
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
                    let mut cfg = config.lock().expect("config mutex poisoned");
                    switch_profile_reconciled(&mut cfg, &canonical)?;
                } else {
                    println!("aborted — no changes made");
                    return Ok(());
                }
            } else {
                let mut cfg = config.lock().expect("config mutex poisoned");
                switch_profile(&mut cfg, &canonical)?;
            }

            // Match the TUI: prime the 5h window if the target is opted in
            // via `auto_start = true`. Cooldown blocks repeated CLI switches
            // from re-kicking inside the same window.
            {
                let _spinner = Spinner::start("clauth: priming usage window…");
                let _ = oauth::auto_start_named(
                    &config,
                    &canonical,
                    &noop_refetch,
                    &noop_activity,
                    &op_sender,
                );
            }
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
