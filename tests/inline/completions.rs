//! Shell-completions feature coverage: the advertised
//! `clauth completions install [shell]` path. `print_script` is a pure
//! shell→script lookup; `install_rc` / `install_fish` write into home-derived
//! paths, so they run under a home sandbox.

use super::*;

#[test]
fn print_script_supports_bash_zsh_fish() {
    for shell in ["bash", "zsh", "fish"] {
        print_script(shell).unwrap_or_else(|_| panic!("{shell} must be supported"));
    }
}

#[test]
fn print_script_rejects_unsupported_shell() {
    let err = print_script("powershell").expect_err("unsupported shell must error");
    assert!(
        err.to_string().contains("unsupported shell"),
        "error must name the unsupported shell",
    );
}

#[cfg(unix)]
use crate::testutil::HomeSandbox;

/// `completions install bash` writes the script under `~/.clauth/completions/`
/// and appends an idempotent `source` line to `~/.bashrc`.
#[cfg(unix)]
#[test]
fn install_bash_writes_script_and_sources_it_in_rc() {
    let home = HomeSandbox::new();
    let home_path = home.home();

    install(Some("bash")).expect("install bash completions");

    let script = home_path
        .join(".clauth")
        .join("completions")
        .join("clauth.bash");
    assert!(
        script.is_file(),
        "the bash completion script must be written"
    );
    assert!(
        std::fs::read_to_string(&script)
            .expect("read script")
            .contains("complete -F _clauth clauth"),
        "the written script must be the bash completion body",
    );

    let rc = std::fs::read_to_string(home_path.join(".bashrc")).expect("read .bashrc");
    assert!(
        rc.contains(&format!("source \"{}\"", script.display())),
        ".bashrc must source the generated completion script",
    );
}

/// Re-running `install` must not append a second `source` line — the rc edit is
/// idempotent (guarded by the existing-line check).
#[cfg(unix)]
#[test]
fn install_bash_is_idempotent_across_reruns() {
    let home = HomeSandbox::new();
    let home_path = home.home();

    install(Some("bash")).expect("first install");
    install(Some("bash")).expect("second install");

    let rc = std::fs::read_to_string(home_path.join(".bashrc")).expect("read .bashrc");
    let count = rc.matches("# clauth completions").count();
    assert_eq!(count, 1, "the rc source block must be written exactly once");
}

/// Fish does not edit an rc file: the script lands in fish's own completions dir.
#[cfg(unix)]
#[test]
fn install_fish_writes_into_fish_completions_dir() {
    let home = HomeSandbox::new();
    let home_path = home.home();

    install(Some("fish")).expect("install fish completions");

    let script = home_path
        .join(".config")
        .join("fish")
        .join("completions")
        .join("clauth.fish");
    assert!(
        script.is_file(),
        "fish completions must be written to the fish completions dir",
    );
    assert!(
        !home_path.join(".bashrc").exists() && !home_path.join(".zshrc").exists(),
        "installing fish must not touch bash/zsh rc files",
    );
}

#[test]
fn install_rejects_unsupported_shell() {
    let err = install(Some("powershell")).expect_err("unsupported shell must error");
    assert!(
        err.to_string().contains("unsupported shell"),
        "error must name the unsupported shell",
    );
}

// The first-launch consent prompt defaults to Yes: an empty answer (bare Enter)
// installs, so the convenient path stays a single keypress.
#[test]
fn answer_is_yes_defaults_to_yes_on_empty() {
    for a in ["", "   ", "\n", "\r\n"] {
        assert!(answer_is_yes(a), "{a:?} (default) must install");
    }
}

#[test]
fn answer_is_yes_accepts_y_and_yes_any_case() {
    for a in ["y", "Y", "yes", "YES", " Yes "] {
        assert!(answer_is_yes(a), "{a:?} must install");
    }
}

#[test]
fn answer_is_yes_declines_on_n_or_other_input() {
    for a in ["n", "N", "no", "nope", "q", "x"] {
        assert!(!answer_is_yes(a), "{a:?} must decline");
    }
}
