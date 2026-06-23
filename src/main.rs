mod actions;
mod claude;
mod claude_json;
mod completions;
mod fallback;
mod format;
mod lock;
mod lockorder;
mod mcp;
mod oauth;
mod platform;
mod plugin_probe;
mod poll;
mod pricing;
mod profile;
mod profile_cache;
mod providers;
mod runtime;
mod spinner;
mod start;
mod status;
mod throughput;
mod tokens;
mod tui;
mod update;
mod usage;
mod which;

#[cfg(test)]
mod testutil;

use anyhow::Result;

use crate::profile::{AppConfig, ThemeName, load_config};
use crate::runtime::Isolation;

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
            anyhow::bail!("usage: clauth start [--isolated] <profile> [claude args...]");
        }
        [cmd, flag] if cmd == "start" && flag == "--isolated" => {
            anyhow::bail!("usage: clauth start --isolated <profile> [claude args...]");
        }
        [cmd, flag, name, rest @ ..] if cmd == "start" && flag == "--isolated" => {
            cmd_start(name, rest, Isolation::Isolated)
        }
        [cmd, name, rest @ ..] if cmd == "start" => cmd_start(name, rest, Isolation::Shared),
        [cmd, ..] if cmd == "run" => anyhow::bail!(
            "`clauth run` isn't a command — for a headless delegate use \
             `clauth start <profile> -p \"<prompt>\"` (or the MCP `run` tool)"
        ),
        [cmd] if cmd == "mcp" => mcp::serve(),
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
                // Drop the flag; split_at gives (0..i, i..) but a contiguous
                // slice over both halves needs allocation — the flag must come
                // before all other args.
                let (_, after) = args.split_at(i);
                return (tier, &after[1..]);
            }
        }
    }
    (None, args)
}

fn cmd_start(name: &str, rest: &[String], isolation: Isolation) -> Result<()> {
    platform::init();
    runtime::gc_stale_runtimes();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    start::run(&config, &canonical, rest, isolation)
}

fn cmd_switch(name: &str) -> Result<()> {
    platform::init();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    actions::switch_profile_cli(config, &canonical)
}

fn cmd_tui(theme_override: Option<tui::theme::Tier>) -> Result<()> {
    platform::init();
    runtime::gc_stale_runtimes();
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
           clauth start [--isolated] <profile> [args]\n                                  \
         launch claude with that profile's settings in a per-profile\n                                  \
         CLAUDE_CONFIG_DIR; --isolated injects creds but drops operator\n                                  \
         memory/plugins/hooks (run in a clean cwd for a blind session);\n                                  \
         extra args go to claude\n  \
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

/// Feature→test traceability map.
#[cfg(test)]
#[path = "../tests/inline/feature_coverage.rs"]
mod feature_coverage;
