mod actions;
mod claude;
mod claude_json;
mod completions;
mod fallback;
mod format;
// macOS-only: Claude Code reads its login from the Keychain, not the credentials
// file, so a switch must also write there. Gated so non-macOS builds stay clean.
#[cfg(target_os = "macos")]
mod keychain;
mod lock;
mod lockorder;
mod mcp;
mod oauth;
mod oauth_login;
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
        [cmd, rest @ ..] if cmd == "login" => match parse_login_args(rest) {
            Some((name, model)) => cmd_login(name, model),
            None => anyhow::bail!("usage: clauth login <profile> [--model <id>]"),
        },
        [cmd, ..] if cmd == "run" => anyhow::bail!(
            "`clauth run` isn't a command — for a headless delegate use \
             `clauth start <profile> -p \"<prompt>\"` (or the MCP `delegate` tool)"
        ),
        [cmd] if cmd == "mcp" => mcp::serve(),
        // Hidden: the bundled PostToolUse `asyncRewake` hook body. Reads the hook
        // payload on stdin, waits for a background delegate, and wakes the model.
        [cmd] if cmd == "mcp-await-job" => mcp::await_job(),
        [name] => cmd_switch(name),
        [] => cmd_tui(theme_override),
        _ => anyhow::bail!(
            "usage: clauth [profile] | clauth start <profile> [claude args...] | clauth login <profile> [--model <id>] | clauth which [--json] | clauth completions <bash|zsh|fish> | clauth completions install [shell]"
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

/// `clauth login`'s args after the `login` token: bare `<profile>` or
/// `<profile> --model <id>`. `None` on any other shape (missing profile,
/// `--model` with no value, an unrecognized flag, extra args) — the caller
/// turns that into one usage bail. Kept as its own pure fn (dispatch's other
/// subcommands hand-roll the match inline) so the shape is unit-testable
/// without invoking `cmd_login`, which opens a real browser.
fn parse_login_args(rest: &[String]) -> Option<(&str, Option<&str>)> {
    match rest {
        // A `--`-prefixed "name" is a typo'd/misplaced flag, not a profile —
        // `clauth login --model` must bail with usage, not create "--model".
        [name] if !name.starts_with("--") => Some((name.as_str(), None)),
        [name, flag, value] if flag == "--model" && !name.starts_with("--") => {
            Some((name.as_str(), Some(value.as_str())))
        }
        _ => None,
    }
}

/// Where `clauth login <name>` lands. An EXISTING profile (matched
/// case-insensitively, carrying its stored canonical spelling) is
/// re-authenticated in place through the issue-#7 overwrite path; any other
/// name creates a fresh profile. Pure, so the routing is unit-testable without
/// `cmd_login`, which opens a real browser.
#[derive(Debug, PartialEq)]
enum LoginRoute {
    /// No profile has this name — mint tokens into a brand-new profile.
    New(String),
    /// A profile already exists under this (canonical) name — mint fresh
    /// tokens and overwrite its credential set in place, keeping its chain
    /// slot, env, and model settings.
    Reauth(String),
}

fn login_route(config: &AppConfig, raw: &str) -> LoginRoute {
    match config.canonical_name(raw.trim()) {
        // Route to the stored canonical spelling, not the typed case variant,
        // so `clauth login ACME` for stored `acme` refreshes the same profile
        // instead of bailing on the case-insensitive collision check.
        Some(existing) => LoginRoute::Reauth(existing),
        // Store the TRIMMED name. Every later lookup (`canonical_name`,
        // `resolve_or_bail`, `switch`) matches without trimming, so a padded
        // `"  new  "` would be unreachable afterwards and a later `login "new"`
        // wouldn't detect the collision, silently making a near-duplicate.
        None => LoginRoute::New(raw.trim().to_string()),
    }
}

/// Parse a reauth-overwrite confirmation with a default-NO policy — the op
/// replaces a profile's stored credentials, so silence must not proceed. Only
/// `y`/`yes` (case-insensitive) confirms.
fn reauth_confirmed(input: &str) -> bool {
    let a = input.trim();
    a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")
}

/// `clauth login <name>` — add a new OAuth account by real browser login, with
/// visible progress, or RE-AUTHENTICATE an existing profile in place (#7).
/// clauth reproduces Claude Code's own PKCE + loopback flow (see `oauth_login`)
/// and writes the minted tokens straight into the profile's
/// `.credentials.json`, so it works identically on every platform — unlike
/// running CC's own `/login`, which on macOS lands only in a per-config-dir
/// hashed Keychain item and leaves the profile file empty (#1/#3).
///
/// A NEW name captures into a fresh profile; an EXISTING name routes through
/// [`actions::overwrite_captured_profile`] — fresh tokens replace the
/// credential set in place (chain slot, env, and model settings survive; stale
/// per-account fetch caches are dropped; when it is the ACTIVE profile the
/// live link is re-run so a running `claude` picks the new login up). Neither
/// path switches to the profile (`clauth <name>` does that). `--model` (any
/// preset alias or a full custom id, same values the Setup tab's model row
/// accepts) is persisted onto the profile after capture, so its sessions route
/// to that model from the first launch. Tokens are never printed — only a
/// sha256 prefix.
fn cmd_login(name: &str, model: Option<&str>) -> Result<()> {
    platform::init();
    let mut config = load_config()?;
    let route = login_route(&config, name);
    let target = match &route {
        LoginRoute::Reauth(existing) => existing.clone(),
        LoginRoute::New(fresh) => {
            actions::validate_profile_name(fresh, &config.names(), None)?;
            fresh.clone()
        }
    };

    let reauth = matches!(route, LoginRoute::Reauth(_));

    if reauth {
        // A reauth overwrites the profile's stored credentials, so guard the
        // typo case (a new account was meant, an existing name was typed) with
        // a confirm. Only when BOTH ends are a TTY: a piped/non-interactive
        // stdin can't be prompted and proceeds, so scripted reauth still works.
        use std::io::{IsTerminal as _, Write as _};
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            print!(
                "clauth: profile '{target}' already exists. Re-authenticating replaces its stored credentials. Continue? [y/N] "
            );
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !reauth_confirmed(&answer) {
                println!("clauth: aborted. '{target}' left unchanged.");
                return Ok(());
            }
        }
        println!("clauth: re-authenticating existing profile '{target}' — opening a browser…");
    } else {
        println!("clauth: opening a browser to log in to a new account for '{target}'…");
    }
    let credentials = oauth_login::login_with(|progress| {
        // The CLI surfaces only the paste-fallback URL; the later milestones
        // are TUI-modal fodder and would just be noise between the prints here.
        if let oauth_login::LoginProgress::AuthorizeUrl(url) = progress {
            println!("\nIf the browser didn't open, visit this URL to authorize:\n{url}\n");
        }
    })?;

    println!(
        "clauth: login complete.\n{}",
        oauth_login::login_summary(&credentials)
    );
    // Seed the identity anchor for unattended mirror adoption (best-effort —
    // a probe failure never fails the login): the account uuid this login
    // authenticates as, cached per profile so `oauth::try_adopt_live_rotation`
    // can verify a diverged live login is the SAME account even after the
    // stored token dies.
    if let Some(tok) = credentials
        .claude_ai_oauth
        .as_ref()
        .map(|o| o.access_token.clone())
        && let Some(id) = crate::usage::fetch_account_uuid(&tok)
    {
        crate::profile_cache::write_profile_cache(
            &target,
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            &id,
        );
    }
    let snapshot = actions::CaptureSnapshot {
        credentials: Some(credentials),
        base_url: None,
        api_key: None,
    };
    if reauth {
        actions::overwrite_captured_profile(&mut config, &target, snapshot)?;
    } else {
        actions::capture_into_profile(&mut config, target.clone(), snapshot)?;
    }
    // Apply the requested default model to the captured profile so its
    // sessions route there from the first launch. On a reauth this is an
    // explicit override — without `--model` the profile's settings survive.
    if let Some(model) = model {
        actions::set_profile_default_model(&mut config, &target, model)?;
    }
    if reauth {
        println!("clauth: re-authenticated '{target}'. Fresh tokens are in place.");
    } else {
        println!("clauth: captured into profile '{target}'. Switch to it with:  clauth {target}");
    }
    Ok(())
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
           clauth login <profile> [--model <id>]\n                                  \
         add a new account via browser OAuth sign-in and capture it into a\n                                  \
         new profile, or re-authenticate an existing one in place (neither\n                                  \
         switches to it); --model sets its default model (opus/sonnet/\n                                  \
         haiku/opusplan or a full model id)\n  \
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

#[cfg(test)]
#[path = "../tests/inline/cli.rs"]
mod tests;
