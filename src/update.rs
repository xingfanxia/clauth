use std::env;
use std::fs;
use std::io::Write;
use std::sync::mpsc::Sender;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

const API_URL: &str = "https://api.github.com/repos/uwuclxdy/clauth/releases/latest";
/// Env var: set to `1` to disable all background update work.
const NO_UPDATE_ENV: &str = "CLAUTH_NO_UPDATE";

/// Outcome of the background update check. Errors are silent; only actionable
/// results are reported.
pub(crate) enum UpdateEvent {
    /// Newer release downloaded and self-installed; effective next launch.
    Installed(String),
    /// Newer release exists but can't be self-applied (cargo install or no
    /// prebuilt asset); user must update manually.
    Available(String),
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns `true` when the update system is active (env var unset or not `"1"`).
pub(crate) fn updates_enabled() -> bool {
    env::var(NO_UPDATE_ENV).as_deref() != Ok("1")
}

/// Spawn a background update check; applies if self-replaceable, toasts result.
/// Returns immediately; errors are silently discarded.
/// Skips all work when `CLAUTH_NO_UPDATE=1`.
pub(crate) fn spawn(tx: Sender<UpdateEvent>) {
    if !updates_enabled() {
        return;
    }
    std::thread::spawn(move || {
        let _ = try_update(&tx);
    });
}

fn try_update(tx: &Sender<UpdateEvent>) -> anyhow::Result<()> {
    let release = fetch_latest()?;

    if !is_newer(&release.tag_name, CURRENT_VERSION) {
        return Ok(());
    }
    let version = release.tag_name.trim_start_matches('v').to_string();

    // Cargo install or unsupported platform: notify only.
    let Some(asset) = asset_name() else {
        let _ = tx.send(UpdateEvent::Available(version));
        return Ok(());
    };
    if is_cargo_installed() {
        let _ = tx.send(UpdateEvent::Available(version));
        return Ok(());
    }

    let Some(url) = release
        .assets
        .iter()
        .find(|a| a.name == asset)
        .map(|a| a.browser_download_url.clone())
    else {
        return Ok(());
    };

    // Derive the sha256sums.txt URL from the binary asset URL.
    // Both assets live in the same release, so we swap the asset name.
    let sums_url = derive_sums_url(&url, asset);

    download_and_replace(&url, &sums_url, asset)?;
    let _ = tx.send(UpdateEvent::Installed(version));
    Ok(())
}

/// Build the `sha256sums.txt` URL from a binary asset URL by replacing the
/// asset filename with `sha256sums.txt`.
fn derive_sums_url(asset_url: &str, asset: &str) -> String {
    // asset_url ends in `/<asset>` — replace trailing filename.
    if let Some(idx) = asset_url.rfind(asset) {
        format!("{}sha256sums.txt", &asset_url[..idx])
    } else {
        // Fallback: append alongside (shouldn't happen with canonical GH URLs).
        format!("{asset_url}/sha256sums.txt")
    }
}

/// Parse a single `sha256sum`-format line: `<64-hex-chars>  <filename>`.
/// Returns `(hex, filename)` or `None` on malformed input.
pub(crate) fn parse_sums_line(line: &str) -> Option<(&str, &str)> {
    // Standard format: 64 hex chars, two spaces, then filename.
    let (hex, rest) = line.split_once("  ")?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((hex, rest.trim_end()))
}

/// Return the expected SHA-256 hex for `asset` from a `sha256sums.txt` body,
/// or `None` when the asset isn't listed.
pub(crate) fn find_expected_sha(sums_text: &str, asset: &str) -> Option<String> {
    sums_text.lines().find_map(|line| {
        let (hex, name) = parse_sums_line(line)?;
        (name == asset).then(|| hex.to_owned())
    })
}

/// Verify that `bytes` hash to `expected_hex` (lowercase SHA-256).
/// Returns `true` on a match, `false` on mismatch or malformed hex string.
pub(crate) fn verify_sha256(bytes: &[u8], expected_hex: &str) -> bool {
    if expected_hex.len() != 64 {
        return false;
    }
    let digest = Sha256::digest(bytes);
    // Hex-string equality check (not constant-time; fine for an integrity
    // compare, do NOT reuse for secret/MAC comparison).
    let actual = digest.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    actual == expected_hex.to_ascii_lowercase()
}

fn make_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .build()
        .into()
}

fn fetch_latest() -> anyhow::Result<Release> {
    let text = make_agent()
        .get(API_URL)
        .header("User-Agent", "clauth-updater")
        .call()
        .map_err(crate::ureq_error::into_anyhow)?
        .body_mut()
        .read_to_string()
        .map_err(crate::ureq_error::into_anyhow)?;

    Ok(serde_json::from_str(&text)?)
}

fn download_and_replace(url: &str, sums_url: &str, asset: &str) -> anyhow::Result<()> {
    let agent = make_agent();

    // 1. Fetch the sums file; treat any error as fail-closed (no integrity = no update).
    let sums_text = agent
        .get(sums_url)
        .header("User-Agent", "clauth-updater")
        .call()
        .map_err(|e| anyhow::anyhow!("sha256sums.txt fetch failed (skipping update): {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| anyhow::anyhow!("sha256sums.txt read failed (skipping update): {e}"))?;

    let expected_hex = find_expected_sha(&sums_text, asset)
        .ok_or_else(|| anyhow::anyhow!("asset {asset} not listed in sha256sums.txt — aborting"))?;

    // 2. Download the asset binary; capped at 10 MB via read_to_vec().
    let bytes = agent
        .get(url)
        .header("User-Agent", "clauth-updater")
        .call()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .body_mut()
        .read_to_vec()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // 3. Integrity check — abort on mismatch.
    if !verify_sha256(&bytes, &expected_hex) {
        anyhow::bail!(
            "SHA-256 mismatch for {asset}: download corrupted or tampered — aborting update"
        );
    }

    // 4. Write to tmp and atomically self-replace.
    let tmp_path = env::temp_dir().join("clauth_update.tmp");
    let _ = fs::remove_file(&tmp_path);

    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755))?;
    }

    // self_replace handles in-place replacement, including Windows.
    self_replace::self_replace(&tmp_path)?;
    let _ = fs::remove_file(&tmp_path);

    Ok(())
}

fn is_cargo_installed() -> bool {
    let Ok(exe) = env::current_exe() else {
        return false;
    };
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    exe.starts_with(home.join(".cargo").join("bin"))
}

fn asset_name() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("clauth-linux-x86_64")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("clauth-macos-x86_64")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("clauth-macos-aarch64")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("clauth-windows-x86_64.exe")
    } else {
        None
    }
}

fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let v = v.trim_start_matches('v');
    let mut it = v.splitn(3, '.');
    Some((
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ))
}

fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(latest), Some(cur)) => latest > cur,
        _ => false,
    }
}

#[cfg(test)]
#[path = "../tests/inline/update.rs"]
mod tests;
