use std::fs;

use anyhow::{Context, Result, bail};

use crate::profile::{home_dir, load_config};

const BASH: &str = r#"_clauth() {
    local cur prev
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    if [ "$COMP_CWORD" -eq 1 ]; then
        local profiles
        profiles=$(clauth __complete 2>/dev/null)
        COMPREPLY=( $(compgen -W "${profiles} start login delete which status daemon doctor completions" -- "${cur}") )
    elif [ "${COMP_WORDS[1]}" = "login" ] && [ "${cur:0:2}" = "--" ]; then
        COMPREPLY=( $(compgen -W "--base-url --api-key --model --new" -- "${cur}") )
    elif [ "${COMP_WORDS[1]}" = "start" ] && [ "${cur:0:2}" = "--" ]; then
        COMPREPLY=( $(compgen -W "--isolated" -- "${cur}") )
    elif [ "$prev" = "--isolated" ]; then
        local profiles
        profiles=$(clauth __complete 2>/dev/null)
        COMPREPLY=( $(compgen -W "${profiles}" -- "${cur}") )
    elif [ "$COMP_CWORD" -eq 2 ] && { [ "$prev" = "start" ] || [ "$prev" = "login" ] || [ "$prev" = "delete" ]; }; then
        local profiles
        profiles=$(clauth __complete 2>/dev/null)
        COMPREPLY=( $(compgen -W "${profiles}" -- "${cur}") )
    elif [ "$COMP_CWORD" -eq 2 ] && { [ "$prev" = "which" ] || [ "$prev" = "status" ]; }; then
        COMPREPLY=( $(compgen -W "--json" -- "${cur}") )
    elif [ "$COMP_CWORD" -eq 2 ] && [ "$prev" = "completions" ]; then
        COMPREPLY=( $(compgen -W "bash zsh fish install" -- "${cur}") )
    elif [ "${COMP_WORDS[1]}" = "delete" ] && [ "${cur:0:2}" = "--" ]; then
        COMPREPLY=( $(compgen -W "--yes -y --force" -- "${cur}") )
    fi
    return 0
}
complete -F _clauth clauth
"#;

const ZSH: &str = r#"#compdef clauth
_clauth() {
    if (( CURRENT == 2 )); then
        local -a profiles
        profiles=("${(@f)$(clauth __complete 2>/dev/null)}")
        _describe 'profile' profiles
        _values 'subcommand' \
            'start[launch claude with that profile]' \
            'login[log in via browser OAuth or an API key]' \
            'delete[remove a profile and its credentials]' \
            'which[print profile owning the loaded credentials]' \
            'status[print the usage / auto-switch snapshot]' \
            'daemon[run the headless scheduler (no TUI)]' \
            'doctor[read-only health check of the daemon + macOS wiring]' \
            'completions[print or install shell completions]'
    elif (( CURRENT == 3 )) && [[ "${words[2]}" == (start|login|delete) ]]; then
        local -a profiles
        profiles=("${(@f)$(clauth __complete 2>/dev/null)}")
        _describe 'profile' profiles
        [[ "${words[2]}" == start ]] && _values 'flag' '--isolated[clean isolated runtime; drops operator config]'
    elif (( CURRENT == 4 )) && [[ "${words[2]}" == start && "${words[3]}" == --isolated ]]; then
        local -a profiles
        profiles=("${(@f)$(clauth __complete 2>/dev/null)}")
        _describe 'profile' profiles
    elif (( CURRENT == 3 )) && [[ "${words[2]}" == (which|status) ]]; then
        _values 'flag' '--json[emit JSON instead of plain name]'
    elif (( CURRENT == 3 )) && [[ "${words[2]}" == completions ]]; then
        _values 'arg' 'bash' 'zsh' 'fish' 'install[install into the shell rc]'
    elif (( CURRENT >= 4 )) && [[ "${words[2]}" == login ]]; then
        _values 'flag' '--base-url[API base url]' '--api-key[API key (prompted echo-off if omitted)]' '--model[set the default model before signing in]' '--new[refuse to touch an existing profile]'
    elif (( CURRENT >= 4 )) && [[ "${words[2]}" == delete ]]; then
        _values 'flag' '--yes[skip the confirm prompt]' '-y[skip the confirm prompt]' '--force[override the live-session guard]'
    fi
}
_clauth "$@"
"#;

const FISH: &str = r#"function __clauth_profiles
    clauth __complete 2>/dev/null
end
complete -c clauth -f
complete -c clauth -f -n __fish_is_first_token -a "(__clauth_profiles)" -d Profile
complete -c clauth -f -n __fish_is_first_token -a start -d "Launch claude with that profile's runtime"
complete -c clauth -f -n __fish_is_first_token -a login -d "Log in via browser OAuth or an API key"
complete -c clauth -f -n __fish_is_first_token -a delete -d "Remove a profile and its credentials"
complete -c clauth -f -n __fish_is_first_token -a which -d "Print profile owning the loaded credentials"
complete -c clauth -f -n __fish_is_first_token -a status -d "Print the usage / auto-switch snapshot"
complete -c clauth -f -n __fish_is_first_token -a daemon -d "Run the headless scheduler (no TUI)"
complete -c clauth -f -n __fish_is_first_token -a doctor -d "Read-only health check of the daemon + macOS wiring"
complete -c clauth -f -n __fish_is_first_token -a completions -d "Print or install shell completions"
complete -c clauth -f -n "__fish_seen_subcommand_from start login delete" -a "(__clauth_profiles)" -d Profile
complete -c clauth -f -n "__fish_seen_subcommand_from start" -a --isolated -d "Clean isolated runtime; drops operator config"
complete -c clauth -f -n "__fish_seen_subcommand_from which status" -a --json -d "Emit JSON"
complete -c clauth -f -n "__fish_seen_subcommand_from completions" -a "bash zsh fish install" -d Shell
complete -c clauth -f -n "__fish_seen_subcommand_from login" -a --base-url -d "API base url"
complete -c clauth -f -n "__fish_seen_subcommand_from login" -a --api-key -d "API key (prompted echo-off if omitted)"
complete -c clauth -f -n "__fish_seen_subcommand_from login" -a --model -d "Set default model before signing in"
complete -c clauth -f -n "__fish_seen_subcommand_from login" -a --new -d "Refuse to touch an existing profile"
complete -c clauth -f -n "__fish_seen_subcommand_from delete" -a --yes -d "Skip the confirm prompt"
complete -c clauth -f -n "__fish_seen_subcommand_from delete" -a -y -d "Skip the confirm prompt"
complete -c clauth -f -n "__fish_seen_subcommand_from delete" -a --force -d "Override the live-session guard"
"#;

pub(crate) fn print_script(shell: &str) -> Result<()> {
    let script = match shell {
        "bash" => BASH,
        "zsh" => ZSH,
        "fish" => FISH,
        _ => bail!("unsupported shell '{shell}', expected: bash, zsh, fish"),
    };
    print!("{script}");
    Ok(())
}

pub(crate) fn print_profile_names() {
    let Ok(config) = load_config() else {
        return;
    };
    for name in config.names() {
        println!("{name}");
    }
}

pub(crate) fn install(shell: Option<&str>) -> Result<()> {
    let shell = match shell {
        Some(s) => s.to_string(),
        None => detect_shell()?,
    };

    match shell.as_str() {
        "bash" => install_rc("bash", BASH, ".bashrc"),
        "zsh" => install_rc("zsh", ZSH, ".zshrc"),
        "fish" => install_fish(),
        s => bail!("unsupported shell '{s}', expected: bash, zsh, fish"),
    }
}

fn detect_shell() -> Result<String> {
    let path = std::env::var("SHELL").context(
        "$SHELL not set; pass the shell explicitly: clauth completions install <bash|zsh|fish>",
    )?;
    let name = path.rsplit('/').next().unwrap_or("");
    match name {
        "bash" | "zsh" | "fish" => Ok(name.to_string()),
        other => bail!(
            "unrecognized shell '{other}' from $SHELL; pass it explicitly: clauth completions install <bash|zsh|fish>"
        ),
    }
}

fn install_rc(shell: &str, script: &str, rc_name: &str) -> Result<()> {
    let home = home_dir()?;
    let completions_dir = home.join(".clauth").join("completions");
    crate::profile::mkdir_700(&completions_dir)?;
    let script_path = completions_dir.join(format!("clauth.{shell}"));
    crate::profile::atomic_write_600(&script_path, script)
        .with_context(|| format!("failed to write {}", script_path.display()))?;

    let rc_path = home.join(rc_name);
    let source_line = format!("source \"{}\"", script_path.display());

    let existing = fs::read_to_string(&rc_path).unwrap_or_default();
    let already = existing.lines().any(|l| l.trim() == source_line);

    if !already {
        let mut new = existing;
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        new.push_str(&format!("\n# clauth completions\n{source_line}\n"));
        fs::write(&rc_path, new)
            .with_context(|| format!("failed to update {}", rc_path.display()))?;
    }

    Ok(())
}

/// Env var: set to `1` to skip the first-launch completions auto-install
/// entirely (only `"1"` opts out, matching `CLAUTH_NO_UPDATE`).
const NO_COMPLETIONS_ENV: &str = "CLAUTH_NO_COMPLETIONS";

fn completions_opt_out() -> bool {
    std::env::var(NO_COMPLETIONS_ENV).as_deref() == Ok("1")
}

/// Outcome of asking the user whether to install completions.
enum Consent {
    /// Install — explicit yes, or the default-Yes empty answer.
    Yes,
    /// User declined — record it so we never ask again.
    No,
    /// Couldn't ask (not a TTY): skip WITHOUT recording, so the next
    /// interactive launch still gets to ask. Never edits an rc unattended.
    CannotAsk,
}

/// Parse a `[Y/n]` answer with a default-Yes policy: empty, `y`, or `yes`
/// (case-insensitive) install; anything else declines.
fn answer_is_yes(input: &str) -> bool {
    let a = input.trim();
    a.is_empty() || a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")
}

/// Ask, on a TTY, before appending a completions `source` line to `~/{rc_name}`.
/// Returns `CannotAsk` (never `Yes`) when stdin/stdout isn't a terminal, so a
/// shell rc is never edited non-interactively.
fn ask_install_completions(rc_name: &str) -> Consent {
    use std::io::{IsTerminal as _, Write as _};
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Consent::CannotAsk;
    }
    print!("clauth: install shell completions? appends a source line to ~/{rc_name} [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Consent::CannotAsk;
    }
    if answer_is_yes(&line) {
        Consent::Yes
    } else {
        Consent::No
    }
}

pub(crate) fn auto_install_once() {
    if completions_opt_out() {
        return;
    }
    let Ok(home) = home_dir() else { return };
    let clauth_dir = home.join(".clauth");
    let sentinel = clauth_dir.join(".completions_installed");
    if sentinel.exists() {
        return;
    }

    let Ok(shell) = detect_shell() else {
        return;
    };

    // bash/zsh append a `source` line to a shell rc → require explicit consent.
    // fish writes only into its own completions dir (the conventional location),
    // so it installs without a prompt.
    let consent = match shell.as_str() {
        "bash" => ask_install_completions(".bashrc"),
        "zsh" => ask_install_completions(".zshrc"),
        _ => Consent::Yes,
    };

    if matches!(consent, Consent::CannotAsk) {
        return; // don't record the sentinel — re-prompt on the next interactive launch
    }

    let _ = crate::profile::mkdir_700(&clauth_dir);
    let _ = crate::profile::atomic_write_600(&sentinel, "");

    if matches!(consent, Consent::Yes)
        && let Err(e) = install(Some(&shell))
    {
        eprintln!("clauth: could not install completions: {e}");
        eprintln!("clauth: run `clauth completions install` later to retry");
    }
}

fn install_fish() -> Result<()> {
    let home = home_dir()?;
    let dir = home.join(".config").join("fish").join("completions");
    fs::create_dir_all(&dir)?;
    let path = dir.join("clauth.fish");
    fs::write(&path, FISH).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/inline/completions.rs"]
mod tests;
