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
    // Holds the STATE rank in the global lock order — pushed once on the
    // outermost acquisition, popped on its drop. None for reentrant calls so
    // the rank is not double-pushed (it is already held by the outer frame).
    _rank: Option<crate::lockorder::RankGuard>,
}

impl StateLock {
    pub(crate) fn acquire() -> Result<Self> {
        let depth = DEPTH.get();
        if depth > 0 {
            // This thread already holds the mutex — increment depth.
            DEPTH.set(
                depth
                    .checked_add(1)
                    .expect("clauth state lock depth overflow"),
            );
            return Ok(Self {
                _thread_guard: None,
                _rank: None,
            });
        }

        // Outermost acquisition: block until we own the thread mutex.
        // `THREAD_LOCK` is `static`, so `.lock()` yields `MutexGuard<'static, _>`
        // directly — storable in `StateLock` with no lifetime laundering.
        // Poison recovery: proceed even if a previous holder panicked.
        let mut guard: std::sync::MutexGuard<'static, Option<File>> = match THREAD_LOCK.lock() {
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

        // Enter the STATE rank on the outermost hold. `config` (rank CONFIG) may
        // already be held — STATE sits inside it; `RankGuard::enter` asserts it.
        let rank = crate::lockorder::RankGuard::enter::<crate::lockorder::rank::State>();

        Ok(Self {
            _thread_guard: Some(guard),
            _rank: Some(rank),
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
            // _thread_guard drops implicitly, unblocking sibling threads.
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
