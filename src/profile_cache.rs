//! Generic per-profile disk-cache IO.
//!
//! Both the OAuth usage layer (`usage/fetch.rs`) and the third-party provider
//! layer (`providers/mod.rs`) persist one JSON file per profile under the same
//! per-profile dir, with the same atomic-write + None-on-error semantics. This
//! module owns that shared IO once; the two layers only differ in their cache
//! filename and the concrete type.

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use serde::{Serialize, de::DeserializeOwned};

/// Filename of the OAuth usage cache, relative to the per-profile dir.
pub(crate) const USAGE_CACHE_FILE: &str = "usage_cache.json";
/// Filename of the third-party provider cache, relative to the per-profile dir.
pub(crate) const THIRD_PARTY_CACHE_FILE: &str = "third_party_cache.json";

/// The account uuid this profile's login last authenticated as (a bare JSON
/// string). Derived data, backfilled on login and on every successful mirror
/// adoption — the identity anchor that lets an unattended adopt refuse a live
/// login belonging to a DIFFERENT account (`oauth::try_adopt_live_rotation`).
pub(crate) const ACCOUNT_ID_CACHE_FILE: &str = "account_id.json";

/// Epoch-ms of this profile's last `/profile` fetch attempt (a bare JSON number).
/// Derived data: the durable half of `usage::fetch`'s once-per-hour-per-profile
/// TTL clock, so a relaunch reuses the cached plan instead of re-pulling
/// `/profile` for every profile at once.
pub(crate) const PROFILE_FETCHED_CACHE_FILE: &str = "profile_fetched.json";

/// Per-profile kick-429 block (`usage::scheduler::KickBlock`): written by the
/// fetching instance so a standdown TUI can mirror the judgment and a restart
/// doesn't forget a live block mid-outage; removed the moment a kick lands.
pub(crate) const KICK_BLOCK_CACHE_FILE: &str = "kick_block.json";

/// Resolve `<profile_dir>/<file>` for `name`. `None` only when the per-profile
/// dir itself can't be resolved (matches the prior per-layer `cache_path`).
pub(crate) fn profile_cache_path(name: &str, file: &str) -> Option<PathBuf> {
    // `profile_dir` (override-aware) rather than raw `dirs::home_dir`, so tests
    // never touch the real `~/.clauth`.
    crate::profile::profile_dir(name).ok().map(|p| p.join(file))
}

/// Read + deserialize `<profile_dir>/<file>`. `None` on missing file or any
/// read/parse error — the caller treats both as "no cache" (matches the prior
/// per-layer loaders exactly).
pub(crate) fn load_profile_cache<T: DeserializeOwned>(name: &str, file: &str) -> Option<T> {
    profile_cache_path(name, file).and_then(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        serde_json::from_str::<T>(&text).ok()
    })
}

/// Atomically write `value` to `<profile_dir>/<file>`. Failures are swallowed
/// (cache is best-effort): a missing parent is created at 0o700, the file at
/// 0o600, via a tmp + rename so a torn write reads as no cache rather than a
/// parse failure.
pub(crate) fn write_profile_cache<T: Serialize>(name: &str, file: &str, value: &T) {
    let Some(path) = profile_cache_path(name, file) else {
        return;
    };
    let Ok(json) = serde_json::to_string(value) else {
        return;
    };
    let _ = crate::profile::atomic_write_600(&path, json.as_bytes());
}

/// Delete `<profile_dir>/<file>`. Best-effort, same contract as the writer: an
/// already-absent file and any removal error alike leave the caller with "no
/// cache", which is the intended post-state either way.
pub(crate) fn remove_profile_cache(name: &str, file: &str) {
    if let Some(path) = profile_cache_path(name, file) {
        let _ = std::fs::remove_file(path);
    }
}

/// Epoch-ms of `<profile_dir>/<file>`'s last write, or `None` when it's absent.
pub(crate) fn profile_cache_mtime_ms(name: &str, file: &str) -> Option<u64> {
    let modified = std::fs::metadata(profile_cache_path(name, file)?)
        .ok()?
        .modified()
        .ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}
