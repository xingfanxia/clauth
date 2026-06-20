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
