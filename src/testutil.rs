//! Shared test-only helpers used across the inline test modules
//! (`tests/inline/*.rs`). Defined once here rather than copied per module so the
//! home-sandbox, mtime, and key-event scaffolding stays in a single place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// RAII home sandbox: acquires `HOME_TEST_LOCK` and redirects `home_dir()` into
/// a tempdir for its lifetime, clearing the override on drop (even on panic).
/// Required for any test that writes into the per-profile tree or creates
/// session dirs, pid files, or rotation locks — otherwise those paths land in
/// the real `~/.clauth`.
pub(crate) struct HomeSandbox {
    // Drop order: tempdir first, then the shared lock.
    _tmp: tempfile::TempDir,
    _guard: std::sync::MutexGuard<'static, ()>,
    home: PathBuf,
}

impl HomeSandbox {
    pub(crate) fn new() -> Self {
        // Untracked HOME_TEST_LOCK acquired first; no RankedMutex/flock is held.
        let guard = crate::profile::HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("create home sandbox");
        let home = tmp.path().to_path_buf();
        crate::profile::set_home_override(home.clone());
        Self {
            _tmp: tmp,
            _guard: guard,
            home,
        }
    }

    /// Path to the sandboxed home directory.
    pub(crate) fn home(&self) -> &Path {
        &self.home
    }
}

impl Drop for HomeSandbox {
    fn drop(&mut self) {
        crate::profile::clear_home_override();
    }
}

/// A minimal `Profile` with every optional field unset — tests fill in what
/// they assert on.
pub(crate) fn blank_profile(name: &str) -> crate::profile::Profile {
    crate::profile::Profile {
        name: name.into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: Default::default(),
        models: Default::default(),
        fallback_threshold: None,
        last_resort: false,
        bell_threshold: None,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: None,
        third_party_usage: None,
    }
}

/// Overwrite a file's modification time — for cache-staleness / tie-break tests.
pub(crate) fn set_mtime(path: &Path, when: SystemTime) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for mtime");
    file.set_modified(when).expect("set_modified");
}

/// A `Press` key event with no modifiers.
pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Collect a `Command`'s queued env overrides: key → `Some(value)` for a set
/// var, key → `None` for a removed one. `get_envs` reflects only the explicit
/// `env`/`env_remove` ops, which is exactly what we assert. No process env or
/// spawn needed, so this is lock-free and non-flaky.
pub(crate) fn env_overrides(cmd: &Command) -> HashMap<String, Option<String>> {
    cmd.get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|s| s.to_string_lossy().into_owned()),
            )
        })
        .collect()
}
