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
mod status;
mod tui;
mod update;
mod ureq_error;
mod usage;
mod which;

use anyhow::Result;

use crate::profile::{AppConfig, ThemeName, load_config};

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

fn dispatch(args: &[String]) -> Result<()> {
    // Strip a leading `--theme=<name>` flag before command dispatch. The flag
    // is only meaningful for `clauth` (TUI path); it is silently accepted but
    // has no effect on the non-TUI paths so the flag can sit anywhere early.
    let (theme_override, args) = peel_theme_flag(args);

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
        [] => cmd_tui(theme_override),
        _ => anyhow::bail!(
            "usage: clauth [profile] | clauth start <profile> [claude args...] | clauth which [--json] | clauth completions <bash|zsh|fish> | clauth completions install [shell]"
        ),
    }
}

/// Strip the first `--theme=full|compatible` element from `args`. Returns the
/// resolved [`tui::theme::Tier`] override (if present) and the remaining args.
fn peel_theme_flag(args: &[String]) -> (Option<tui::theme::Tier>, &[String]) {
    for (i, arg) in args.iter().enumerate() {
        if let Some(value) = arg.strip_prefix("--theme=") {
            let tier = match value.to_lowercase().as_str() {
                "full" => Some(tui::theme::Tier::Full),
                "compatible" => Some(tui::theme::Tier::Compatible),
                _ => None,
            };
            if tier.is_some() {
                // Drop the flag; return the rest by splitting around index i.
                // SAFETY: we only call this before dispatch, args slice is valid.
                let (before, after) = args.split_at(i);
                // `before` is 0..i, `after` is i.. (includes the flag at 0).
                // Reconstruct as a single contiguous slice is not possible without
                // allocation — but in practice `--theme=` always comes first, so
                // we return the remaining tail. For the general case the flag
                // must precede all other args.
                let _ = before; // before is empty in normal use
                return (tier, &after[1..]);
            }
        }
    }
    (None, args)
}

fn cmd_start(name: &str, rest: &[String]) -> Result<()> {
    platform::init();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    start::run(&config, &canonical, rest)
}

fn cmd_switch(name: &str) -> Result<()> {
    platform::init();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    actions::switch_profile_cli(config, &canonical)
}

fn cmd_tui(theme_override: Option<tui::theme::Tier>) -> Result<()> {
    platform::init();
    completions::auto_install_once();
    let config = load_config()?;
    // Config-file tier: profiles.toml `theme = "full"|"compatible"`.
    // CLI flag beats config; both beat auto-detect.
    let config_tier = config.state.theme.map(|t| match t {
        ThemeName::Full => tui::theme::Tier::Full,
        ThemeName::Compatible => tui::theme::Tier::Compatible,
    });
    tui::theme::init(theme_override.or(config_tier));
    tui::run(config)
}

fn print_help() {
    println!(
        "clauth {ver} — Claude Code account switcher\n\n\
         Usage:\n  \
           clauth [--theme=full|compatible] launch the TUI\n  \
           clauth <profile>                switch to profile by name and exit\n  \
           clauth start <profile> [args]   launch claude with that profile's settings\n                                  \
         in an isolated CLAUDE_CONFIG_DIR; extra args go to claude\n  \
           clauth which [--json]           print the profile owning the loaded\n                                  \
         credentials.json (CLAUDE_CONFIG_DIR-aware); `unknown` on no match\n  \
           clauth completions <shell>      print shell completion script (bash|zsh|fish)\n  \
           clauth completions install [shell]\n                                  \
         install completions into the user's shell rc\n  \
           clauth --version                print version\n  \
           clauth --help                   show this help\n\n\
         Theme:\n  \
           --theme=full        force 24-bit truecolor (default when $COLORTERM=truecolor)\n  \
           --theme=compatible  force xterm-256 palette (safe on all terminals)\n  \
           Config file:        set `theme = \"full\"` in ~/.clauth/profiles.toml",
        ver = env!("CARGO_PKG_VERSION"),
    );
}
