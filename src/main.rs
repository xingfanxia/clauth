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
            Some(args) => cmd_login(args),
            None => anyhow::bail!(
                "usage: clauth login <profile> [--base-url <url>] [--api-key <key>] [--model <id>]"
            ),
        },
        [cmd, rest @ ..] if cmd == "delete" => match parse_delete_args(rest) {
            Some((name, yes)) => cmd_delete(name, yes),
            None => anyhow::bail!("usage: clauth delete <profile> [--yes]"),
        },
        [cmd, ..] if cmd == "run" => anyhow::bail!(
            "`clauth run` isn't a command; for a headless delegate use \
             `clauth start <profile> -p \"<prompt>\"` (or the MCP `delegate` tool)"
        ),
        [cmd] if cmd == "mcp" => mcp::serve(),
        // Hidden: the bundled PostToolUse `asyncRewake` hook body. Reads the hook
        // payload on stdin, waits for a background delegate, and wakes the model.
        [cmd] if cmd == "mcp-await-job" => mcp::await_job(),
        [name] => cmd_switch(name),
        [] => cmd_tui(theme_override),
        _ => anyhow::bail!(
            "usage: clauth [profile] | clauth start [--isolated] <profile> [args] | clauth login <profile> [--base-url <url>] [--api-key <key>] [--model <id>] | clauth delete <profile> [--yes] | clauth which [--json] | clauth completions <bash|zsh|fish> | clauth completions install [shell]"
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

/// `clauth login`'s parsed args after the `login` token: one profile name plus
/// any of `--model <id>`, `--base-url <url>`, `--api-key <key>` (each takes the
/// next token as its value), in any order. Presence of `--base-url` or
/// `--api-key` selects API-key mode; both absent selects browser OAuth (the
/// original behaviour). `None` on any other shape (missing profile, a flag with
/// no value or a `--`-prefixed value, an unrecognized flag, two positional
/// names) — the caller turns that into one usage bail. Kept as its own pure fn
/// so the shape is unit-testable without invoking `cmd_login`, which opens a
/// real browser or reads a key.
#[derive(Debug, PartialEq)]
struct LoginArgs<'a> {
    name: &'a str,
    model: Option<&'a str>,
    base_url: Option<&'a str>,
    api_key: Option<&'a str>,
}

impl LoginArgs<'_> {
    /// API-key mode: capture a base_url + api_key pair instead of browser OAuth.
    fn is_api_mode(&self) -> bool {
        self.base_url.is_some() || self.api_key.is_some()
    }
}

fn parse_login_args(rest: &[String]) -> Option<LoginArgs<'_>> {
    let mut name: Option<&str> = None;
    let mut model: Option<&str> = None;
    let mut base_url: Option<&str> = None;
    let mut api_key: Option<&str> = None;

    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].as_str();
        // A known value flag consumes the next token as its value.
        let slot = match arg {
            "--model" => Some(&mut model),
            "--base-url" => Some(&mut base_url),
            "--api-key" => Some(&mut api_key),
            _ => None,
        };
        if let Some(slot) = slot {
            // Missing value, or a value that is itself a flag (`login acme
            // --model --base-url` is a forgotten model value) → bail.
            let value = rest.get(i + 1)?.as_str();
            if value.starts_with("--") {
                return None;
            }
            *slot = Some(value);
            i += 2;
            continue;
        }
        // Any other `--` token is an unrecognized flag.
        if arg.starts_with("--") {
            return None;
        }
        // Positional token: the profile name. A second one is a typo'd extra,
        // not a second profile.
        if name.is_some() {
            return None;
        }
        name = Some(arg);
        i += 1;
    }

    Some(LoginArgs {
        name: name?,
        model,
        base_url,
        api_key,
    })
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

/// Prompt `[y/N]` before a reauth overwrites a profile's stored credentials.
/// Non-TTY stdin proceeds (a piped script can't be prompted), matching the
/// OAuth reauth contract. `is_api` tailors the copy (endpoint + key vs tokens).
fn confirm_reauth(target: &str, is_api: bool) -> Result<bool> {
    use std::io::{IsTerminal as _, Write as _};
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(true);
    }
    let object = if is_api {
        "endpoint + API key"
    } else {
        "stored credentials"
    };
    print!(
        "clauth: profile '{target}' already exists. Re-authenticating replaces its {object}. Continue? [y/N] "
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(reauth_confirmed(&answer))
}

/// Collect the base_url + api_key pair for API-key mode. Each value comes from
/// its `--flag` when given; otherwise a prompt: base_url on a normal echo'ing
/// line, api_key echo-off (it's a secret). A non-TTY stdin that still owes a
/// value bails — a script must pass both flags explicitly.
fn collect_api_endpoint(
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    use std::io::{IsTerminal as _, Write as _};
    let interactive = std::io::stdin().is_terminal();

    let base_url = match base_url {
        Some(u) => Some(u.to_string()),
        None => {
            if !interactive {
                anyhow::bail!("non-interactive stdin: pass --base-url (and --api-key) explicitly");
            }
            print!("Base URL: ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                anyhow::bail!("base url is required for an API account");
            }
            Some(trimmed)
        }
    };

    let api_key = match api_key {
        Some(k) => {
            eprintln!(
                "clauth: warning: --api-key is visible in shell history and process listings; prefer the prompt"
            );
            Some(k.to_string())
        }
        None => {
            if !interactive {
                anyhow::bail!("non-interactive stdin: pass --api-key explicitly");
            }
            let k = rpassword::prompt_password("API key: ")
                .map_err(|e| anyhow::anyhow!("failed to read API key: {e}"))?;
            let k = k.trim().to_string();
            if k.is_empty() {
                anyhow::bail!("api key is required for an API account");
            }
            Some(k)
        }
    };

    Ok((base_url, api_key))
}

/// Run the browser OAuth flow (preamble, authorize-URL paste fallback, minted
/// tokens, login summary, identity-anchor seed) and wrap it in a capture
/// snapshot. Shared by `cmd_login`'s new and reauth OAuth arms so the two stay
/// in lockstep.
fn run_oauth_browser(reauth: bool, target: &str) -> Result<actions::CaptureSnapshot> {
    if reauth {
        println!("clauth: re-authenticating existing profile '{target}', opening a browser…");
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
            target,
            crate::profile_cache::ACCOUNT_ID_CACHE_FILE,
            &id,
        );
    }
    Ok(actions::CaptureSnapshot {
        credentials: Some(credentials),
        base_url: None,
        api_key: None,
    })
}

/// `clauth login <name> [--base-url <url>] [--api-key <key>] [--model <id>]` —
/// add a new account or re-authenticate an existing one in place (#7). The auth
/// method is flag-selected: bare (no `--base-url`/`--api-key`) runs the browser
/// OAuth flow (`oauth_login`) and writes the minted tokens straight into the
/// profile's `.credentials.json`, identically on every platform; passing either
/// endpoint flag switches to API-key mode and captures a base_url + api_key pair
/// instead, prompting (echo-off for the key) for whatever a flag omitted.
///
/// A NEW name captures into a fresh profile; an EXISTING name routes through
/// [`actions::overwrite_captured_profile`] — the fresh credential set (tokens OR
/// endpoint + key) replaces the old in place (chain slot, env, and model
/// settings survive; stale per-account fetch caches are dropped; when it is the
/// ACTIVE profile the live link is re-run so a running `claude` picks the new
/// login up). A reauth that crosses types (OAuth ↔ API) is allowed: the
/// snapshot overwrites all three of credentials/base_url/api_key, so the old
/// type's leftovers are cleared. Neither path switches to the profile (`clauth
/// <name>` does that). `--model` is persisted onto the profile after capture.
/// Tokens are never printed — only a sha256 prefix.
fn cmd_login(args: LoginArgs<'_>) -> Result<()> {
    platform::init();
    let mut config = load_config()?;
    let route = login_route(&config, args.name);
    let target = match &route {
        LoginRoute::Reauth(existing) => existing.clone(),
        LoginRoute::New(fresh) => {
            actions::validate_profile_name(fresh, &config.names(), None)?;
            fresh.clone()
        }
    };
    let reauth = matches!(route, LoginRoute::Reauth(_));
    let is_api = args.is_api_mode();

    // Confirm a reauth BEFORE collecting anything (browser or key prompt): a
    // declined overwrite must not open a browser or read a secret.
    if reauth && !confirm_reauth(&target, is_api)? {
        println!("clauth: aborted. '{target}' left unchanged.");
        return Ok(());
    }

    if reauth {
        let snapshot = if is_api {
            let (base_url, api_key) = collect_api_endpoint(args.base_url, args.api_key)?;
            actions::CaptureSnapshot {
                credentials: None,
                base_url,
                api_key,
            }
        } else {
            run_oauth_browser(true, &target)?
        };
        actions::overwrite_captured_profile(&mut config, &target, snapshot)?;
        // On a reauth `--model` is an explicit override; without it the
        // profile's existing model settings survive.
        if let Some(model) = args.model {
            actions::set_profile_default_model(&mut config, &target, model)?;
        }
        let what = if is_api { "endpoint + key" } else { "tokens" };
        println!("clauth: re-authenticated '{target}'. Fresh {what} are in place.");
    } else if is_api {
        // A new API profile goes through `create_blank_profile` (the TUI's
        // path), NOT `capture_into_profile`: the latter auto-activates the
        // first profile and links credentials, but an API account carries no
        // credentials.json and its base_url/api_key reach the live
        // settings.json only via a switch — so auto-activating would mark it
        // "active" before it's wired. The user switches explicitly (the print
        // below), which writes settings.json. `create_blank_profile` also
        // takes the model inline, so no separate model write is needed here.
        let (base_url, api_key) = collect_api_endpoint(args.base_url, args.api_key)?;
        actions::create_blank_profile(
            &mut config,
            target.clone(),
            base_url,
            api_key,
            args.model.map(str::to_string),
        )?;
        println!("clauth: captured into profile '{target}'. Switch to it with:  clauth {target}");
    } else {
        let snapshot = run_oauth_browser(false, &target)?;
        actions::capture_into_profile(&mut config, target.clone(), snapshot)?;
        // Apply the requested default model so the captured profile's sessions
        // route there from the first launch.
        if let Some(model) = args.model {
            actions::set_profile_default_model(&mut config, &target, model)?;
        }
        println!("clauth: captured into profile '{target}'. Switch to it with:  clauth {target}");
    }
    Ok(())
}

/// `clauth delete <name> [--yes]`'s args after the `delete` token: one profile
/// name plus an optional `--yes`/`-y` (anywhere). `None` on any other shape
/// (missing name, an unrecognized flag, two names). Pure, so unit-testable
/// without invoking `cmd_delete`, which touches the filesystem.
fn parse_delete_args(rest: &[String]) -> Option<(&str, bool)> {
    let mut name: Option<&str> = None;
    let mut yes = false;
    for arg in rest {
        match arg.as_str() {
            "--yes" | "-y" => yes = true,
            a if a.starts_with("--") => return None,
            a => {
                if name.is_some() {
                    return None;
                }
                name = Some(a);
            }
        }
    }
    Some((name?, yes))
}

/// `clauth delete <name> [--yes]` — remove a profile and all its credentials
/// (the whole on-disk profile dir + state + caches), OAuth or API-key. Prompts
/// `[y/N]` on a TTY unless `--yes`; a non-TTY stdin skips the prompt so scripts
/// can delete, matching the reauth contract. If the deleted profile was active,
/// its live `~/.claude/.credentials.json` link is cleared too.
fn cmd_delete(name: &str, yes: bool) -> Result<()> {
    platform::init();
    let mut config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    if !yes {
        use std::io::{IsTerminal as _, Write as _};
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            print!("clauth: delete profile '{canonical}' and all its credentials? [y/N] ");
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !reauth_confirmed(&answer) {
                println!("clauth: aborted. '{canonical}' left in place.");
                return Ok(());
            }
        }
    }
    let was_active = config.is_active(&canonical);
    actions::delete_profile(&mut config, &canonical)?;
    if was_active {
        println!("clauth: deleted profile '{canonical}' (was active; live credentials cleared).");
    } else {
        println!("clauth: deleted profile '{canonical}'.");
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
        "clauth {ver}: Claude Code account switcher\n\n\
         Usage:\n  \
           clauth [--theme=full|compatible] launch the TUI\n  \
           clauth <profile>                switch to profile by name and exit\n  \
           clauth start [--isolated] <profile> [args]\n                                  \
         launch claude with that profile's settings in a per-profile\n                                  \
         CLAUDE_CONFIG_DIR; --isolated injects creds but drops operator\n                                  \
         memory/plugins/hooks (run in a clean cwd for a blind session);\n                                  \
         extra args go to claude\n  \
           clauth login <profile> [--base-url <url>] [--api-key <key>] [--model <id>]\n                                  \
         add a new account, or re-authenticate an existing one in place\n                                  \
         (neither switches to it). Bare = browser OAuth; pass --base-url\n                                  \
         or --api-key to capture an API-key account instead (a missing\n                                  \
         value is prompted; the key is read echo-off). --model sets its\n                                  \
         default model (opus/sonnet/haiku/opusplan or a full model id)\n  \
           clauth delete <profile> [--yes]\n                                  \
         remove a profile and all its credentials; --yes skips the confirm\n  \
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
