mod actions;
mod claude;
mod claude_json;
mod completions;
mod daemon;
mod fallback;
mod format;
// macOS-only: Claude Code reads its login from the Keychain, not the credentials
// file, so a switch must also write there. Gated so non-macOS builds stay clean.
#[cfg(target_os = "macos")]
mod keychain;
mod lock;
mod lockorder;
mod logline;
mod mcp;
mod oauth;
mod oauth_login;
mod platform;
mod plugin_probe;
mod poll;
mod pricing;
mod profile;
mod profile_cache;
mod profile_json;
mod providers;
mod runtime;
mod sessions;
mod sessions_cli;
mod spinner;
mod start;
mod status;
mod throughput;
mod token_ledger;
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(exit_code(dispatch(&args)));
}

/// A usage error (bad flag/args) for the sessions-surface commands. Distinct
/// from a runtime failure so [`exit_code`] can map it to process exit 2, while a
/// genuine error (including "no sessions found") stays exit 1.
#[derive(Debug)]
pub(crate) struct UsageError(pub(crate) String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// Build a [`UsageError`] as an `anyhow::Error` for a dispatch arm to return.
fn usage_error(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UsageError(msg.into()))
}

/// Map a dispatch outcome to a process exit code: 0 on success, 2 for a
/// [`UsageError`] (bad flag/args), 1 for any other failure. Prints the error
/// exactly as anyhow's `Result` `Termination` did (`Error: {:?}`), so the
/// message surface is unchanged now that `main` maps the code itself.
pub(crate) fn exit_code(result: Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e:?}");
            if e.downcast_ref::<UsageError>().is_some() {
                2
            } else {
                1
            }
        }
    }
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
        [cmd, rest @ ..] if cmd == "start" => match parse_start_args(rest) {
            Some(a) => cmd_start(a.name, a.claude_args, a.isolation, a.rescue_override),
            None => anyhow::bail!(
                "usage: clauth start [--isolated] [--rescue|--no-rescue] <profile> [claude args...]\n\
                 (--rescue/--no-rescue require --isolated)"
            ),
        },
        [cmd, rest @ ..] if cmd == "login" => match parse_login_args(rest) {
            Some(args) => cmd_login(args),
            None => anyhow::bail!(
                "usage: clauth login <profile> [--base-url <url>] [--api-key <key>] [--setup-token [--yes]] [--model <id>]"
            ),
        },
        [cmd, rest @ ..] if cmd == "delete" => match parse_delete_args(rest) {
            Some((name, yes, force)) => cmd_delete(name, yes, force),
            None => anyhow::bail!("usage: clauth delete <profile> [--yes] [--force]"),
        },
        [cmd, ..] if cmd == "run" => anyhow::bail!(
            "`clauth run` isn't a command; for a headless delegate use \
             `clauth start <profile> -p \"<prompt>\"` (or the MCP `delegate` tool)"
        ),
        [cmd] if cmd == "mcp" => mcp::serve(),
        // Hidden: the bundled PostToolUse `asyncRewake` hook body. Reads the hook
        // payload on stdin, waits for a background delegate, and wakes the model.
        [cmd] if cmd == "mcp-await-job" => mcp::await_job(),
        [cmd] if cmd == "sessions" => sessions_cli::run_sessions(false),
        [cmd, flag] if cmd == "sessions" && flag == "--json" => sessions_cli::run_sessions(true),
        [cmd, ..] if cmd == "sessions" => Err(usage_error("usage: clauth sessions [--json]")),
        [cmd, target] if cmd == "resume" => sessions_cli::run_resume(target, None),
        [cmd, target, flag, value] if cmd == "resume" && flag == "--profile" => {
            sessions_cli::run_resume(target, Some(value))
        }
        [cmd, ..] if cmd == "resume" => Err(usage_error(
            "usage: clauth resume <id|latest> [--profile <name>]",
        )),
        [cmd, target] if cmd == "info" => sessions_cli::run_info(target),
        [cmd, ..] if cmd == "info" => Err(usage_error("usage: clauth info <id|latest>")),
        [cmd] if cmd == "daemon" => daemon::serve(),
        [cmd, flag] if cmd == "status" && flag == "--json" => daemon::status_oneshot(),
        [cmd, ..] if cmd == "status" => {
            anyhow::bail!("usage: clauth status --json");
        }
        [name] => cmd_switch(name),
        [] => cmd_tui(theme_override),
        // Unrecognized invocation: show the full command list, not a stale subset.
        _ => {
            print_help();
            Ok(())
        }
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

fn cmd_start(
    name: &str,
    rest: &[String],
    isolation: Isolation,
    rescue_override: Option<bool>,
) -> Result<()> {
    platform::init();
    runtime::gc_stale_runtimes();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    start::run(&config, &canonical, rest, isolation, None, rescue_override)
}

/// `clauth start`'s parsed args after the `start` token: leading clauth flags
/// (`--isolated`, and — only under `--isolated` — `--rescue`/`--no-rescue`), the
/// profile name, then any trailing tokens passed straight through to `claude`.
/// The clauth flags must precede the name; the first non-flag token is the name
/// and everything after it is `claude`'s, so a passthrough `--resume`/`-p` is
/// never mistaken for a clauth flag. `None` on a missing name, or a
/// `--rescue`/`--no-rescue` without `--isolated` — the caller maps that to one
/// usage bail. Pure, so the shape is unit-testable without spawning `claude`.
#[derive(Debug, PartialEq)]
struct StartArgs<'a> {
    name: &'a str,
    isolation: Isolation,
    rescue_override: Option<bool>,
    claude_args: &'a [String],
}

fn parse_start_args(rest: &[String]) -> Option<StartArgs<'_>> {
    let mut isolated = false;
    let mut rescue_override: Option<bool> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--isolated" => isolated = true,
            "--rescue" => rescue_override = Some(true),
            "--no-rescue" => rescue_override = Some(false),
            // First non-clauth-flag token is the profile name; everything after
            // it belongs to `claude`.
            _ => break,
        }
        i += 1;
    }
    let name = rest.get(i)?.as_str();
    // Rescue lifts a throwaway isolated store into the global one; a shared start
    // already writes there, so the flags without `--isolated` are a user error,
    // rejected rather than silently no-op'd.
    if rescue_override.is_some() && !isolated {
        return None;
    }
    Some(StartArgs {
        name,
        isolation: if isolated {
            Isolation::Isolated
        } else {
            Isolation::Shared
        },
        rescue_override,
        claude_args: &rest[i + 1..],
    })
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
    /// CLA-SPLIT capture flow: read a `claude setup-token` mint and write it as
    /// the profile's `session-token.json` sidecar instead of any OAuth/API login.
    setup_token: bool,
    /// With `--setup-token` only: replace an existing sidecar without the
    /// `[y/N]` prompt (required on a non-TTY stdin, where nothing can confirm).
    yes: bool,
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
    let mut setup_token = false;
    let mut yes = false;

    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].as_str();
        // Boolean flags consume nothing.
        match arg {
            "--setup-token" => {
                setup_token = true;
                i += 1;
                continue;
            }
            "--yes" | "-y" => {
                yes = true;
                i += 1;
                continue;
            }
            _ => {}
        }
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

    // `--setup-token` is its own capture mode — combining it with the API-key
    // pair is a contradiction, and `--yes` means nothing outside it.
    if setup_token && (base_url.is_some() || api_key.is_some()) {
        return None;
    }
    if yes && !setup_token {
        return None;
    }

    Some(LoginArgs {
        name: name?,
        model,
        base_url,
        api_key,
        setup_token,
        yes,
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
        // A flag value gets the same trim + empty-reject as the prompt, so
        // `--base-url ""` or a space-padded value can't slip through unvalidated.
        Some(u) => {
            let u = u.trim();
            if u.is_empty() {
                anyhow::bail!("base url is required for an API account");
            }
            Some(u.to_string())
        }
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
            let k = k.trim();
            if k.is_empty() {
                anyhow::bail!("api key is required for an API account");
            }
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
    let outcome = oauth_login::login_with(|progress| {
        // The CLI surfaces only the paste-fallback URL; the later milestones
        // are TUI-modal fodder and would just be noise between the prints here.
        if let oauth_login::LoginProgress::AuthorizeUrl(url) = progress {
            println!("\nIf the browser didn't open, visit this URL to authorize:\n{url}\n");
        }
    })?;
    println!(
        "clauth: login complete.\n{}",
        oauth_login::login_summary(&outcome.credentials)
    );
    // The uuid the login's own verification probe saw rides the snapshot to
    // whichever action commits it, which is what anchors the profile — so
    // `oauth::try_adopt_live_rotation` can prove a diverged live login is the
    // SAME account even after the stored token dies. Seeding it here instead
    // would anchor an account whose commit may still fail.
    Ok(actions::CaptureSnapshot {
        credentials: Some(outcome.credentials),
        base_url: None,
        api_key: None,
        account_uuid: outcome.account_uuid,
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

    // CLA-SPLIT capture flow: `--setup-token` writes the profile's
    // session-token sidecar and touches NOTHING else — the usage OAuth pair,
    // env, chain slot, and model settings all survive, so it needs none of
    // the reauth/overwrite machinery below.
    if args.setup_token {
        return cmd_login_setup_token(&mut config, &target, reauth, args.model, args.yes);
    }

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
                // An api-key login authenticates no Anthropic account.
                account_uuid: None,
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

/// `clauth login <name> --setup-token [--yes] [--model <id>]` — capture a
/// `claude setup-token` mint into the profile's `session-token.json` sidecar
/// (CLA-SPLIT), replacing today's fill-it-by-hand step. The token is read
/// echo-off on a TTY (it's a bearer credential) or as one line from a piped
/// stdin (so a GUI/script can drive the capture); its value is never echoed
/// or logged. Additive: nothing else about the profile moves, and the sidecar
/// takes effect on the next switch — this deliberately does not touch the
/// live slot, so capturing can never sign a running session out.
fn cmd_login_setup_token(
    config: &mut profile::AppConfig,
    target: &str,
    exists: bool,
    model: Option<&str>,
    yes: bool,
) -> Result<()> {
    use std::io::IsTerminal as _;
    let interactive = std::io::stdin().is_terminal();

    // Replacing an existing sidecar re-points every future switch at the new
    // token — confirm like the other in-place replacements. A fresh capture
    // (no sidecar yet) is additive and needs no ceremony.
    if claude::session_token_status(target).is_some() && !yes {
        if !interactive {
            anyhow::bail!(
                "'{target}' already has a long-lived token; pass --yes to replace it non-interactively"
            );
        }
        print!("Replace the stored long-lived token for '{target}'? [y/N] ");
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !reauth_confirmed(&answer) {
            println!("clauth: aborted. '{target}' left unchanged.");
            return Ok(());
        }
    }

    let raw = if interactive {
        println!("clauth: capturing a long-lived token for '{target}'.");
        println!("  1. in another terminal, run:  claude setup-token");
        println!("  2. complete the browser flow it opens");
        println!("  3. paste the minted token below (input stays hidden)");
        rpassword::prompt_password("Setup token: ")
            .map_err(|e| anyhow::anyhow!("failed to read the token: {e}"))?
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line
    };
    let token = claude::validate_setup_token(&raw)?;

    // A brand-new name gets a blank profile first (no credentials — the
    // sidecar IS its login; a usage OAuth pair can be added later with a
    // normal `clauth login`).
    if !exists {
        actions::create_blank_profile(
            config,
            target.to_string(),
            None,
            None,
            model.map(str::to_string),
        )?;
    } else if let Some(model) = model {
        actions::set_profile_default_model(config, target, model)?;
    }

    let expires_at = claude::write_session_token(target, &token, crate::usage::now_ms() as i64)?;
    let days = (expires_at - crate::usage::now_ms() as i64) / 86_400_000;
    println!(
        "clauth: long-lived token installed for '{target}' · assumed to expire in ~{days}d \
         (`claude setup-token` mints last about a year)."
    );
    println!(
        "clauth: it takes effect on the next switch:  clauth {target}{}",
        if exists {
            ""
        } else {
            "\nclauth: for usage polling, also add an OAuth pair later:  clauth login <name>"
        }
    );
    Ok(())
}

/// `clauth delete <name> [--yes] [--force]`'s args after the `delete` token: one
/// profile name plus optional `--yes`/`-y` and `--force` (anywhere). `None` on
/// any other shape (missing name, an unrecognized flag, two names). Pure, so
/// unit-testable without invoking `cmd_delete`, which touches the filesystem.
///
/// The two flags are distinct: `--yes` skips the `[y/N]` confirm; `--force`
/// overrides the live-session guard. `--yes` alone does NOT override the guard.
fn parse_delete_args(rest: &[String]) -> Option<(&str, bool, bool)> {
    let mut name: Option<&str> = None;
    let mut yes = false;
    let mut force = false;
    for arg in rest {
        match arg.as_str() {
            "--yes" | "-y" => yes = true,
            "--force" => force = true,
            a if a.starts_with("--") => return None,
            a => {
                if name.is_some() {
                    return None;
                }
                name = Some(a);
            }
        }
    }
    Some((name?, yes, force))
}

/// `clauth delete <name> [--yes] [--force]` — remove a profile and all its
/// credentials (the whole on-disk profile dir + state + caches), OAuth or
/// API-key. Prompts `[y/N]` on a TTY unless `--yes`. Delete is an irreversible
/// `remove_dir_all`, so unlike a reauth a non-TTY stdin does NOT get an implicit
/// yes: it must pass `--yes`, else the delete is refused. A profile held by a
/// live `clauth start` session is refused unless `--force` (independent of
/// `--yes`). If the deleted profile was active, its live
/// `~/.claude/.credentials.json` link and settings.json endpoint are cleared.
fn cmd_delete(name: &str, yes: bool, force: bool) -> Result<()> {
    platform::init();
    let mut config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    if !yes {
        use std::io::{IsTerminal as _, Write as _};
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            anyhow::bail!(
                "refusing to delete '{canonical}' without confirmation; pass --yes for a non-interactive delete"
            );
        }
        print!("clauth: delete profile '{canonical}' and all its credentials? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !reauth_confirmed(&answer) {
            println!("clauth: aborted. '{canonical}' left in place.");
            return Ok(());
        }
    }
    let was_active = config.is_active(&canonical);
    actions::delete_profile(&mut config, &canonical, force)?;
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
        "clauth {ver}: launcher and account manager for claude code\n\n\
         Usage:\n  \
           clauth [--theme=full|compatible]\n                                  \
         launch the TUI\n  \
           clauth <profile>                switch to profile by name and exit\n  \
           clauth start [--isolated] [--rescue|--no-rescue] <profile> [args]\n                                  \
         launch claude with that profile's settings in a per-profile\n                                  \
         CLAUDE_CONFIG_DIR; --isolated injects creds but drops operator\n                                  \
         memory/plugins/hooks (run in a clean cwd for a blind session);\n                                  \
         --rescue/--no-rescue (isolated only) override the auto_rescue\n                                  \
         setting, lifting the run's transcripts into the global store;\n                                  \
         extra args go to claude\n  \
           clauth login <profile> [--base-url <url>] [--api-key <key>] [--setup-token [--yes]] [--model <id>]\n                                  \
         add a new account, or re-authenticate an existing one in place\n                                  \
         (neither switches to it). Bare = browser OAuth; pass --base-url\n                                  \
         or --api-key to capture an API-key account instead (a missing\n                                  \
         value is prompted; the key is read echo-off). --setup-token\n                                  \
         captures a `claude setup-token` mint into the profile's\n                                  \
         long-lived session-token sidecar (pasted echo-off, or piped on\n                                  \
         stdin; --yes replaces an existing one unprompted). --model sets\n                                  \
         its default model (opus/sonnet/haiku/opusplan or a full model id)\n  \
           clauth delete <profile> [--yes|-y] [--force]\n                                  \
         remove a profile and all its credentials; --yes (-y) skips the\n                                  \
         confirm, --force overrides the live-session guard\n  \
           clauth which [--json]           print the profile owning the loaded\n                                  \
         .credentials.json (CLAUDE_CONFIG_DIR-aware); `unknown` on no match\n  \
           clauth sessions [--json]        list Claude Code sessions as a table; --json\n                                  \
         emits a stable newest-first array (exit 0/1/2)\n  \
           clauth resume <id|latest> [--profile <name>]\n                                  \
         resume a session under a chosen profile (prompts on a TTY,\n                                  \
         defaulting to the session's last-ran profile; --profile forces)\n  \
           clauth info <id|latest>         print the resume command, workspace, and\n                                  \
         on-disk storage path for a session (never launches)\n  \
           clauth daemon                   run the headless scheduler with no TUI: refresh\n                                  \
         usage, auto-switch on exhaustion, and write ~/.clauth/status.json\n  \
           clauth status --json            print the current usage / auto-switch snapshot\n                                  \
         as JSON (same shape the daemon writes)\n  \
           clauth mcp                      run the stdio MCP server (claude code\n                                  \
         launches this)\n  \
           clauth completions <shell>      print shell completion script (bash|zsh|fish)\n  \
           clauth completions install [shell]\n                                  \
         install completions into the user's shell rc\n  \
           clauth --version                print version\n  \
           clauth --help                   show this help\n\n\
         Theme:\n  \
           --theme=full        force 24-bit truecolor (default when $COLORTERM=truecolor or 24bit)\n  \
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
