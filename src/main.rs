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

use anyhow::Result;

use crate::profile::{AppConfig, load_config};

fn resolve_or_bail(config: &AppConfig, name: &str) -> Result<String> {
    config.canonical_name(name).ok_or_else(|| {
        let available = config.names().join(", ");
        anyhow::anyhow!("profile '{name}' not found\navailable: {available}")
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    dispatch(&args)
}

/// Routes parsed CLI args to the matching per-command handler. Each arm either
/// returns the handler's `Result` or falls through to the TUI (`[]`).
fn dispatch(args: &[String]) -> Result<()> {
    match args {
        [cmd, sub] if cmd == "completions" && sub == "install" => completions::install(None),
        [cmd, sub, shell] if cmd == "completions" && sub == "install" => {
            completions::install(Some(shell))
        }
        [cmd, shell] if cmd == "completions" => completions::print_script(shell),
        [cmd] if cmd == "__complete" => {
            completions::print_profile_names();
            Ok(())
        }
        [cmd] if cmd == "--help" || cmd == "-h" => {
            print_help();
            Ok(())
        }
        [cmd] if cmd == "--version" || cmd == "-V" => {
            println!("clauth {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        [cmd] if cmd == "which" => which::run(false),
        [cmd, flag] if cmd == "which" && flag == "--json" => which::run(true),
        [cmd, _, ..] if cmd == "which" => {
            anyhow::bail!("usage: clauth which [--json]");
        }
        [cmd] if cmd == "start" => {
            anyhow::bail!("usage: clauth start <profile> [claude args...]");
        }
        [cmd, name, rest @ ..] if cmd == "start" => cmd_start(name, rest),
        [name] => cmd_switch(name),
        [] => cmd_tui(),
        _ => anyhow::bail!(
            "usage: clauth [profile] | clauth start <profile> [claude args...] | clauth which [--json] | clauth completions <bash|zsh|fish> | clauth completions install [shell]"
        ),
    }
}

/// `clauth start <profile> [claude args...]` — spawn `claude` against the
/// profile's isolated runtime.
fn cmd_start(name: &str, rest: &[String]) -> Result<()> {
    platform::init();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    start::run(&config, &canonical, rest)
}

/// `clauth <profile>` — switch the active account to `name` and exit.
fn cmd_switch(name: &str) -> Result<()> {
    platform::init();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    actions::switch_profile_cli(config, &canonical)
}

/// `clauth` (no args) — launch the interactive TUI.
fn cmd_tui() -> Result<()> {
    platform::init();
    completions::auto_install_once();
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
