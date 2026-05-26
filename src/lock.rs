//! Cross-process serialization of state mutations.
//!
//! All disk writes that touch shared clauth state (profiles.toml, per-profile
//! config/credentials, ~/.claude/settings.json, .credentials.json symlink) run
//! under an exclusive advisory file lock on ~/.clauth/.lock. This stops two
//! concurrent clauth instances from interleaving read-modify-write cycles and
//! losing each other's changes, racing OAuth refresh-token rotations, or
//! clobbering the active-profile symlink.
//!
//! The lock is re-entrant within the same thread so high-level actions
//! (e.g. `switch_profile`) can take the lock and still call helpers that take
//! it themselves without deadlocking. Two different threads of the same process
//! calling `with_state_lock` concurrently are fully serialized — only one
//! executes its closure at a time.

use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::profile::clauth_dir;

const LOCK_FILENAME: &str = ".lock";

// Serializes all threads of this process across the full closure duration.
// The guard is stored in the outermost StateLock and dropped only when that
// StateLock drops, so no second thread can enter while any thread is inside.
static THREAD_LOCK: Mutex<Option<File>> = Mutex::new(None);

// Per-thread reentrancy depth. Non-zero means this thread already holds
// THREAD_LOCK and must not try to re-acquire it (non-reentrant Mutex).
thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

pub(crate) struct StateLock {
    // Non-None only for the outermost acquisition on this thread.
    // Holds THREAD_LOCK for the full closure lifetime; None for reentrant calls.
    _thread_guard: Option<std::sync::MutexGuard<'static, Option<File>>>,
}

impl StateLock {
    pub(crate) fn acquire() -> Result<Self> {
        let depth = DEPTH.get();
        if depth > 0 {
            // This thread already holds the mutex — just increment depth.
            DEPTH.set(
                depth
                    .checked_add(1)
                    .expect("clauth state lock depth overflow"),
            );
            return Ok(Self {
                _thread_guard: None,
            });
        }

        // Outermost acquisition: block until we own the thread mutex.
        // Poison recovery: if a previous holder panicked we still proceed.
        let mut guard = match THREAD_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        // Open/create the flock file if not already held.
        if guard.is_none() {
            let dir = clauth_dir()?;
            std::fs::create_dir_all(&dir).context("Failed to create ~/.clauth")?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join(LOCK_FILENAME))
                .context("Failed to open clauth state lock file")?;
            // Blocking; releases when the holder drops its lock or exits.
            file.lock().context("Failed to acquire clauth state lock")?;
            *guard = Some(file);
        }

        DEPTH.set(1);

        // Extend the guard's lifetime to 'static. This is sound: THREAD_LOCK
        // is a 'static Mutex, so its contents live for the program lifetime.
        // We hold the guard inside StateLock and drop it in Drop before any
        // other cleanup, so the borrow is valid for as long as Self exists.
        let guard: std::sync::MutexGuard<'static, Option<File>> =
            // SAFETY: THREAD_LOCK is 'static; the guard is valid for 'static.
            unsafe { std::mem::transmute(guard) };

        Ok(Self {
            _thread_guard: Some(guard),
        })
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let depth = DEPTH.get();
        let new_depth = depth.saturating_sub(1);
        DEPTH.set(new_depth);

        if new_depth == 0 {
            // Outermost unwind. Close the flock file so other processes can
            // acquire it, then release the thread mutex so other threads of
            // this process can enter. Both happen when _thread_guard drops.
            if let Some(ref mut g) = self._thread_guard {
                **g = None; // close the File → flock released
            }
            // _thread_guard (mutex guard) drops implicitly, unblocking sibling threads.
        }
        // Reentrant calls have _thread_guard = None; nothing extra to do.
    }
}

/// Run `f` while holding the cross-process state lock. Re-entrant within the
/// same thread; serializes concurrent calls from different threads.
pub(crate) fn with_state_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = StateLock::acquire()?;
    f()
}

#[cfg(test)]
#[path = "../tests/inline/lock.rs"]
mod tests;
