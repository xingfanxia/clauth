mod actions;
mod claude;
mod claude_json;
mod codex;
mod completions;
mod daemon;
mod doctor;
mod fallback;
mod fallback_config;
mod format;
// macOS-only: Claude Code reads its login from the Keychain, not the credentials
// file, so a switch must also write there. Gated so non-macOS builds stay clean.
#[cfg(target_os = "macos")]
mod keychain;
mod lock;
mod lockorder;
mod logline;
mod loopback;
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
mod proxy;
mod runtime;
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

use anyhow::{Context as _, Result};

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
                "usage: clauth login <profile> [--base-url <url>] [--api-key <key>] [--setup-token [--yes]] [--model <id>] [--new] [--codex [--browser]]"
            ),
        },
        [cmd, rest @ ..] if cmd == "delete" => match parse_delete_args(rest) {
            Some((name, yes, force)) => cmd_delete(name, yes, force),
            None => anyhow::bail!("usage: clauth delete <profile> [--yes] [--force]"),
        },
        [cmd, name] if cmd == "resume" => cmd_resume(name),
        [cmd, ..] if cmd == "resume" => {
            anyhow::bail!("usage: clauth resume <codex-profile>");
        }
        [cmd, rest @ ..] if cmd == "fallback" => cmd_fallback(rest),
        [cmd, rest @ ..] if cmd == "proxy" => cmd_proxy(rest),
        [cmd, ..] if cmd == "run" => anyhow::bail!(
            "`clauth run` isn't a command; for a headless delegate use \
             `clauth start <profile> -p \"<prompt>\"` (or the MCP `delegate` tool)"
        ),
        [cmd] if cmd == "mcp" => mcp::serve(),
        // Hidden: the bundled PostToolUse `asyncRewake` hook body. Reads the hook
        // payload on stdin, waits for a background delegate, and wakes the model.
        [cmd] if cmd == "mcp-await-job" => mcp::await_job(),
        [cmd] if cmd == "daemon" => daemon::serve(),
        [cmd] if cmd == "doctor" => doctor::run(),
        [cmd, flag] if cmd == "status" && flag == "--json" => daemon::status_oneshot(),
        [cmd, ..] if cmd == "status" => {
            anyhow::bail!("usage: clauth status --json");
        }
        [name] => cmd_switch(name),
        [] => cmd_tui(theme_override),
        _ => anyhow::bail!(
            "usage: clauth [profile] | clauth start [--isolated] <profile> [args] | clauth login <profile> [--base-url <url>] [--api-key <key>] [--model <id>] [--new] [--codex] | clauth delete <profile> [--yes] [--force] | clauth which [--json] | clauth completions <bash|zsh|fish> | clauth completions install [shell]"
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

/// `clauth fallback <op> …` — chain edits over the `fallback_config`
/// primitives (CDX-4 C1: membership ops route by the profile's harness into
/// the matching chain, so this one surface edits BOTH chains). The daemon
/// picks external `profiles.toml` edits up on its mtime watch, exactly like a
/// hand edit; nothing here needs the socket.
fn cmd_fallback(rest: &[String]) -> Result<()> {
    const USAGE: &str = "usage: clauth fallback list | add <profile> | remove <profile> | \
                         up <profile> | down <profile> | threshold <profile> <pct> | \
                         last-resort <profile> <on|off>";
    let mut config = load_config()?;
    match rest {
        [sub] if sub == "list" => {
            let line = |names: &[crate::profile::ProfileName]| {
                if names.is_empty() {
                    "(empty)".to_string()
                } else {
                    names
                        .iter()
                        .map(|n| n.as_str())
                        .collect::<Vec<_>>()
                        .join(" → ")
                }
            };
            println!("claude chain: {}", line(&config.state.fallback_chain));
            println!("codex  chain: {}", line(&config.state.codex_fallback_chain));
            Ok(())
        }
        [sub, name] if sub == "add" => {
            if fallback_config::add(&mut config, name)? {
                println!("clauth: added '{name}' to its harness's fallback chain");
            } else {
                println!("clauth: '{name}' is already a chain member");
            }
            Ok(())
        }
        [sub, name] if sub == "remove" => {
            if fallback_config::remove(&mut config, name)? {
                println!("clauth: removed '{name}' from its chain");
            } else {
                println!("clauth: '{name}' was not a chain member");
            }
            Ok(())
        }
        [sub, name] if sub == "up" || sub == "down" => {
            let Some(dir) = fallback_config::MoveDir::parse(sub) else {
                anyhow::bail!("{USAGE}"); // unreachable: guarded by the match arm
            };
            if fallback_config::move_member(&mut config, name, dir)? {
                println!("clauth: moved '{name}' {sub}");
            } else {
                println!("clauth: '{name}' did not move (not a member, or at the boundary)");
            }
            Ok(())
        }
        [sub, name, pct] if sub == "threshold" => {
            let value: f64 = pct
                .parse()
                .map_err(|_| anyhow::anyhow!("threshold must be a number, got '{pct}'"))?;
            fallback_config::set_threshold(&mut config, name, value)?;
            println!("clauth: '{name}' switch threshold set to {value}%");
            Ok(())
        }
        [sub, name, onoff] if sub == "last-resort" => {
            let on = match onoff.as_str() {
                "on" => true,
                "off" => false,
                _ => anyhow::bail!("{USAGE}"),
            };
            fallback_config::set_last_resort(&mut config, name, on)?;
            println!(
                "clauth: '{name}' last-resort mark {}",
                if on { "set" } else { "cleared" }
            );
            Ok(())
        }
        _ => anyhow::bail!("{USAGE}"),
    }
}

/// `clauth proxy [--port N]` — run the CDX-5 localhost injection proxy;
/// `clauth proxy --print-config [--port N]` — print the `config.toml` block
/// to paste (clauth never edits the live config). Opt-in; live upstream is
/// AX-manual acceptance.
fn cmd_proxy(rest: &[String]) -> Result<()> {
    let mut port = proxy::DEFAULT_PROXY_PORT;
    let mut print_only = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--print-config" => {
                print_only = true;
                i += 1;
            }
            "--port" => {
                let value = rest
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--port needs a value"))?;
                port = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid port: {value}"))?;
                i += 2;
            }
            other => anyhow::bail!(
                "usage: clauth proxy [--port N] | clauth proxy --print-config [--port N] \
                 (got '{other}')"
            ),
        }
    }
    if print_only {
        proxy::print_config(port);
        Ok(())
    } else {
        platform::init();
        proxy::run(port)
    }
}

fn cmd_start(name: &str, rest: &[String], isolation: Isolation) -> Result<()> {
    platform::init();
    runtime::gc_stale_runtimes();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    // CDX-1b: a codex profile starts `codex` against its isolated CODEX_HOME.
    if config.find(&canonical).is_some_and(|p| p.is_codex()) {
        if isolation == Isolation::Isolated {
            anyhow::bail!(
                "codex starts are always isolated (their own CODEX_HOME) — drop --isolated"
            );
        }
        return start::run_codex(&canonical, rest);
    }
    start::run(&config, &canonical, rest, isolation)
}

/// `clauth login`'s parsed args after the `login` token: one profile name plus
/// any of `--model <id>`, `--base-url <url>`, `--api-key <key>` (each takes the
/// next token as its value) and the bare `--new` flag, in any order. Presence
/// of `--base-url` or `--api-key` selects API-key mode; both absent selects
/// browser OAuth (the original behaviour). `None` on any other shape (missing
/// profile, a flag with no value or a `--`-prefixed value, an unrecognized
/// flag, two positional names) — the caller turns that into one usage bail.
/// Kept as its own pure fn so the shape is unit-testable without invoking
/// `cmd_login`, which opens a real browser or reads a key.
#[derive(Debug, PartialEq)]
struct LoginArgs<'a> {
    name: &'a str,
    model: Option<&'a str>,
    base_url: Option<&'a str>,
    api_key: Option<&'a str>,
    /// `--new`: refuse to reauth an existing profile — the race-proof CREATE
    /// for non-TTY callers (a menu-bar app never sees the confirm prompt).
    new_only: bool,
    /// `--codex`: capture the live `~/.codex/auth.json` into a codex-harness
    /// profile instead of a browser OAuth login (CDX-1 T5). Exclusive with
    /// the claude-shaped flags (`--model`/`--base-url`/`--api-key`).
    codex: bool,
    /// `--browser` (with `--codex`): mint a fresh codex login via the PKCE
    /// loopback flow into the profile store, never touching the live
    /// `~/.codex/auth.json` (CDX-3 R5). Meaningless without `--codex`.
    browser: bool,
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
    let mut new_only = false;
    let mut codex = false;
    let mut browser = false;
    let mut setup_token = false;
    let mut yes = false;

    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].as_str();
        // `--new` is a bare flag: it takes no value. A duplicate is a typo,
        // not idempotent emphasis — bail like any other malformed shape.
        if arg == "--new" {
            if new_only {
                return None;
            }
            new_only = true;
            i += 1;
            continue;
        }
        if arg == "--codex" {
            if codex {
                return None;
            }
            codex = true;
            i += 1;
            continue;
        }
        if arg == "--browser" {
            if browser {
                return None;
            }
            browser = true;
            i += 1;
            continue;
        }
        if arg == "--setup-token" {
            if setup_token {
                return None;
            }
            setup_token = true;
            i += 1;
            continue;
        }
        if arg == "--yes" || arg == "-y" {
            if yes {
                return None;
            }
            yes = true;
            i += 1;
            continue;
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

    // `--codex` captures the live codex login verbatim — the claude-shaped
    // flags have no meaning there, so their combination is a usage error.
    if codex && (model.is_some() || base_url.is_some() || api_key.is_some()) {
        return None;
    }
    // `--browser` only modifies a `--codex` login (the claude login is always
    // a browser flow) — bare `--browser` is a usage error, not a synonym.
    if browser && !codex {
        return None;
    }
    // `--setup-token` is its own capture mode — combining it with the API-key
    // pair, the codex capture, or `--new` is a contradiction, and `--yes`
    // means nothing outside it.
    if setup_token && (base_url.is_some() || api_key.is_some() || codex || new_only) {
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
        new_only,
        codex,
        browser,
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
/// tokens, login summary, identity-anchor probe) and wrap it in a capture
/// snapshot. Shared by `cmd_login`'s new and reauth OAuth arms so the two stay
/// in lockstep. Takes `config` for the CAP-3 sibling-ownership check below.
fn run_oauth_browser(
    config: &AppConfig,
    reauth: bool,
    target: &str,
) -> Result<actions::CaptureSnapshot> {
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
    // Probe the identity anchor for unattended mirror adoption (best-effort —
    // a probe failure never fails the login): the account uuid+email this
    // login authenticates as, cached per profile so
    // `oauth::try_adopt_live_rotation` can verify a diverged live login is the
    // SAME account even after the stored token dies. The identity rides the
    // snapshot (CAP-2) so the capture writes it as the anchor — writing it
    // here first was the 2026-07-12 pollution: the capture re-anchored from
    // the UNRELATED live login's hint, clobbering this authoritative probe.
    let probed = outcome
        .credentials
        .claude_ai_oauth
        .as_ref()
        .map(|o| o.access_token.clone())
        .and_then(|tok| crate::usage::fetch_account_identity(&tok));
    // CAP-3: refuse to store an account a SIBLING profile already holds — two
    // profiles polling one account is a self-inflicted rate-limit pin
    // (2026-07-12, twice: a blind capture, then a wrong-account re-login).
    // Nothing has been written yet, so the refusal is side-effect-free; the
    // freshly minted tokens are simply discarded.
    if let Some(id) = &probed
        && let Some(owner) = actions::account_owner(config, id, target)
    {
        let who = id.email.as_deref().unwrap_or(&id.uuid);
        anyhow::bail!(
            "this browser login is {who}, and profile '{owner}' already holds that \
             account — storing it twice would make both profiles double-poll it \
             into a rate limit. Nothing was saved. To refresh that account, run \
             `clauth login {owner}`; to put a DIFFERENT account into '{target}', \
             sign claude.com out in the browser, log into that account, and rerun."
        );
    }
    let identity = probed.map_or(
        actions::CaptureIdentity::Unknown,
        actions::CaptureIdentity::Known,
    );
    Ok(actions::CaptureSnapshot {
        credentials: Some(outcome.credentials),
        base_url: None,
        api_key: None,
        identity,
    })
}

/// `clauth login <name> [--base-url <url>] [--api-key <key>] [--model <id>]
/// [--new]` — add a new account or re-authenticate an existing one in place
/// (#7). The auth method is flag-selected: bare (no `--base-url`/`--api-key`)
/// runs the browser OAuth flow (`oauth_login`) and writes the minted tokens
/// straight into the profile's `.credentials.json`, identically on every
/// platform; passing either endpoint flag switches to API-key mode and captures
/// a base_url + api_key pair instead, prompting (echo-off for the key) for
/// whatever a flag omitted.
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
/// `--new` refuses the reauth route entirely. Tokens are never printed — only
/// a sha256 prefix.
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

    // CDX-1: harness immutability, reverse direction. Without this guard a
    // bare `clauth login <codex-profile>` falls into the claude reauth path,
    // writing claude credentials into a codex profile — a corrupt hybrid that
    // also re-enters the Anthropic fetch legs (breaking the §0.1 exclusion
    // invariant). The forward direction (`--codex` at a claude profile) is
    // guarded inside `codex_capture_into_profile`; the writer-level backstop
    // lives in `overwrite_captured_profile`. Ahead of the `--setup-token`
    // branch so a claude-shaped sidecar can never land in a codex profile
    // either.
    if reauth && !args.codex && config.find(&target).is_some_and(|p| p.is_codex()) {
        anyhow::bail!(
            "profile '{target}' is a codex profile — re-auth it with: clauth login {target} --codex"
        );
    }

    // CLA-SPLIT capture flow: `--setup-token` writes the profile's
    // session-token sidecar and touches NOTHING else — the usage OAuth pair,
    // env, chain slot, and model settings all survive, so it needs none of
    // the reauth/overwrite machinery below.
    if args.setup_token {
        return cmd_login_setup_token(&mut config, &target, reauth, args.model, args.yes);
    }

    // `--new` pins the CREATE semantics: refuse to touch an existing profile
    // instead of routing to the reauth overwrite. This is the race-proof
    // collision guard for non-TTY callers (a menu-bar app spawning
    // `clauth login` gets no confirm prompt, and any UI-side pre-check is a
    // TOCTOU against this process's freshly-loaded config) — the refusal
    // happens HERE, against current state, so a name minted out-of-band a
    // moment ago can never be silently re-authenticated.
    if args.new_only && reauth {
        anyhow::bail!(
            "profile '{target}' already exists and --new forbids re-authenticating it. \
             Rerun without --new to refresh '{target}', or pick a different name."
        );
    }

    // Confirm a reauth BEFORE collecting anything (browser or key prompt): a
    // declined overwrite must not open a browser or read a secret.
    if reauth && !confirm_reauth(&target, is_api)? {
        println!("clauth: aborted. '{target}' left unchanged.");
        return Ok(());
    }

    // CDX-3 R5: `--codex --browser` mints a fresh codex login via the PKCE
    // loopback flow straight into the profile store — the live
    // ~/.codex/auth.json and the codex active slot are never touched.
    if args.codex && args.browser {
        let bytes = codex::login::browser_login_snapshot(|p| match p {
            codex::login::CodexLoginProgress::AuthorizeUrl(url) => {
                println!("clauth: opening the browser for a codex login…");
                println!("  if it doesn't open, paste this URL yourself:\n  {url}");
            }
            codex::login::CodexLoginProgress::ExchangingCode => {
                println!("clauth: login callback received — exchanging the code…");
            }
        })?;
        actions::codex_store_browser_login(&mut config, &target, &bytes)?;
        println!(
            "clauth: stored the new codex login in profile '{target}' (the live codex \
             login is unchanged). Switch to it with:  clauth {target}"
        );
        return Ok(());
    }

    // CDX-1 T5: `--codex` captures the live ~/.codex/auth.json — no browser,
    // no prompt collection. The actions layer owns every guard (store mode,
    // live presence, account dedup, harness immutability).
    if args.codex {
        actions::codex_capture_into_profile(&mut config, &target)?;
        println!(
            "clauth: captured the live codex login into profile '{target}'. \
             Switch codex accounts with:  clauth {target}"
        );
        return Ok(());
    }

    if reauth {
        let snapshot = if is_api {
            let (base_url, api_key) = collect_api_endpoint(args.base_url, args.api_key)?;
            actions::CaptureSnapshot {
                credentials: None,
                base_url,
                api_key,
                // No OAuth identity to anchor: an API-key endpoint carries no
                // claude.ai account. Unknown drops a stale anchor left by a
                // previous OAuth incarnation of this profile (CAP-2).
                identity: actions::CaptureIdentity::Unknown,
            }
        } else {
            run_oauth_browser(&config, true, &target)?
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
        let snapshot = run_oauth_browser(&config, false, &target)?;
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
    if claude::has_session_token(target) && !yes {
        if !interactive {
            anyhow::bail!(
                "'{target}' already has a session token; pass --yes to replace it non-interactively"
            );
        }
        print!("Replace the stored session token for '{target}'? [y/N] ");
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !reauth_confirmed(&answer) {
            println!("clauth: aborted. '{target}' left unchanged.");
            return Ok(());
        }
    }

    let raw = if interactive {
        println!("clauth: capturing a long-lived session token for '{target}'.");
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
        "clauth: session token installed for '{target}' · assumed to expire in ~{days}d \
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
    // CDX-1 T5: `clauth <name>` stays THE switch verb — the target's harness
    // picks the path. The claude path is untouched.
    if config.find(&canonical).is_some_and(|p| p.is_codex()) {
        return cmd_switch_codex(config, &canonical);
    }
    actions::switch_profile_cli(config, &canonical)
}

/// CLI codex switch: loss-free by default; a FOREIGN live login (matching no
/// stored codex profile) asks `[y/N]` whether to archive it to quarantine and
/// continue. Non-TTY refuses with the actionable message instead — a script
/// must not silently displace a login clauth doesn't own.
fn cmd_switch_codex(mut config: AppConfig, canonical: &str) -> Result<()> {
    use std::io::{IsTerminal as _, Write as _};

    let mut policy = actions::ForeignLivePolicy::Refuse;
    if actions::codex_live_is_foreign(&config)?
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
    {
        print!(
            "clauth: the live codex login matches no stored profile. Archive it to \
             ~/.clauth/quarantine and switch anyway? [y/N] "
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !reauth_confirmed(&answer) {
            println!("clauth: aborted. Live codex login left in place.");
            return Ok(());
        }
        policy = actions::ForeignLivePolicy::Archive;
    }

    let report = actions::codex_switch_profile(&mut config, canonical, policy)?;
    if let Some(owner) = &report.adopted_back {
        println!("clauth: adopted the refreshed live login back into '{owner}' first.");
    }
    if let Some(path) = &report.archived {
        println!(
            "clauth: archived the outgoing live login to {}.",
            path.display()
        );
    }
    println!("clauth: codex now uses '{canonical}'.");
    if codex::codex_processes_running() {
        println!(
            "clauth: note — running codex sessions keep their current account until they \
             exit; the switch applies to new sessions."
        );
    }
    Ok(())
}

/// CDX-1c semi-seamless carryover: switch the live codex login to `name`,
/// then run `codex resume --last` in this terminal — the most recent
/// conversation continues under the new account (codex re-reads auth.json
/// fresh at startup, and `store=false` means the resumed context is resent
/// whole, so the serving account can change mid-conversation at a session
/// boundary). The claude side needs no analogue: its live hot-swap already
/// carries running sessions across.
fn cmd_resume(name: &str) -> Result<()> {
    platform::init();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    if !config.find(&canonical).is_some_and(|p| p.is_codex()) {
        anyhow::bail!(
            "'{canonical}' is a claude profile — `clauth resume` is the codex carryover \
             (a claude switch hot-swaps running sessions already)"
        );
    }
    cmd_switch_codex(config, &canonical)?;
    let status = std::process::Command::new("codex")
        .args(["resume", "--last"])
        .status()
        .context("failed to launch `codex resume --last` (is codex on PATH?)")?;
    std::process::exit(status.code().unwrap_or(1));
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
         extra args go to claude. A CODEX profile instead launches\n                                  \
         `codex` in its own CODEX_HOME (profile chain, isolated\n                                  \
         history; always isolated — no --isolated flag)\n  \
           clauth login <profile> [--base-url <url>] [--api-key <key>] [--setup-token [--yes]] [--model <id>] [--new] [--codex [--browser]]\n                                  \
         add a new account, or re-authenticate an existing one in place\n                                  \
         (neither switches to it). Bare = browser OAuth; pass --base-url\n                                  \
         or --api-key to capture an API-key account instead (a missing\n                                  \
         value is prompted; the key is read echo-off). --setup-token\n                                  \
         captures a `claude setup-token` mint into the profile's\n                                  \
         long-lived session-token sidecar (pasted echo-off, or piped on\n                                  \
         stdin; --yes replaces an existing one unprompted). --model sets\n                                  \
         its default model (opus/sonnet/haiku/opusplan or a full model\n                                  \
         id); --new refuses to touch an existing profile (race-proof\n                                  \
         create for non-TTY callers); --codex captures the live ~/.codex\n                                  \
         login (OpenAI Codex CLI) into a codex profile instead — no\n                                  \
         browser; add --browser to mint a NEW codex login via the\n                                  \
         PKCE flow straight into the store (live login untouched);\n                                  \
         `clauth <profile>` then switches codex accounts too\n  \
           clauth delete <profile> [--yes|-y] [--force]\n                                  \
         remove a profile and all its credentials; --yes (-y) skips the\n                                  \
         confirm, --force overrides the live-session guard\n  \
           clauth resume <codex-profile>   switch the codex login, then run\n                                  \
         `codex resume --last` — the latest conversation continues under\n                                  \
         the new account (semi-seamless carryover)\n  \
           clauth fallback list | add|remove|up|down <profile> |\n                                  \
         threshold <profile> <pct> | last-resort <profile> on|off\n                                  \
         edit the auto-switch chains; membership ops route by the\n                                  \
         profile's harness (claude chain vs codex chain)\n  \
           clauth proxy [--port N]         run the codex injection proxy — point codex at\n                                  \
         it (clauth proxy --print-config) for true in-session codex\n                                  \
         account fallback: a mid-conversation 429 rotates to the next\n                                  \
         chain account and replays before codex sees a byte\n  \
           clauth which [--json]           print the profile owning the loaded\n                                  \
         credentials.json (CLAUDE_CONFIG_DIR-aware); `unknown` on no match\n  \
           clauth daemon                   run the headless scheduler with no TUI: refresh\n                                  \
         usage, auto-switch on exhaustion, and write ~/.clauth/status.json\n                                  \
         (the read format for the menu-bar app)\n  \
           clauth status --json            print the current usage / auto-switch snapshot\n                                  \
         as JSON (same shape the daemon writes)\n  \
           clauth doctor                   read-only health check of the daemon + macOS\n                                  \
         wiring (LaunchAgent, lock, socket, Keychain grant, version skew)\n  \
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
