//! Cross-process serialization of state mutations.
//!
//! All disk writes that touch shared clauth state (profiles.toml, per-profile
//! config/credentials, ~/.claude/settings.json, .credentials.json symlink) run
//! under an exclusive advisory file lock on ~/.clauth/.lock. This stops two
//! concurrent clauth instances from interleaving read-modify-write cycles and
//! losing each other's changes, racing OAuth refresh-token rotations, or
//! clobbering the active-profile symlink.
//!
//! The lock is re-entrant within the same process so high-level actions
//! (e.g. `switch_profile`) can take the lock and still call helpers that take
//! it themselves without deadlocking. Cross-process semantics come from
//! `flock(2)` (unix) / `LockFileEx` (windows); the kernel releases the lock
//! automatically if the holder crashes.
//!
//! ## Pattern
//!
//! ```ignore
//! lock::with_state_lock(|| {
//!     // read-modify-write of shared state
//!     Ok(())
//! })
//! ```

use std::fs::{File, OpenOptions};
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::profile::clauth_dir;

const LOCK_FILENAME: &str = ".lock";

struct Inner {
    file: Option<File>,
    depth: u32,
}

static LOCK: Mutex<Inner> = Mutex::new(Inner {
    file: None,
    depth: 0,
});

/// RAII guard. Released on drop. Construct via `acquire`.
pub(crate) struct StateLock {
    _private: (),
}

impl StateLock {
    pub(crate) fn acquire() -> Result<Self> {
        let mut guard = LOCK.lock().expect("clauth state lock mutex poisoned");
        if guard.depth == 0 {
            let dir = clauth_dir()?;
            std::fs::create_dir_all(&dir).context("Failed to create ~/.clauth")?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join(LOCK_FILENAME))
                .context("Failed to open clauth state lock file")?;
            // Blocking; releases when another process drops its lock or exits.
            // Held briefly by every clauth process (milliseconds per write),
            // so contention is rare.
            file.lock().context("Failed to acquire clauth state lock")?;
            guard.file = Some(file);
        }
        guard.depth = guard
            .depth
            .checked_add(1)
            .expect("clauth state lock depth overflow");
        Ok(Self { _private: () })
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let mut guard = match LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.depth = guard.depth.saturating_sub(1);
        if guard.depth == 0 {
            // Dropping the File closes its FD, which releases the advisory
            // lock for other processes.
            guard.file = None;
        }
    }
}

/// Run `f` while holding the cross-process state lock. Re-entrant.
pub(crate) fn with_state_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = StateLock::acquire()?;
    f()
}
