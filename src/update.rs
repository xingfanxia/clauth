use std::env;
use std::fs;
use std::io::{Read, Write};
use std::time::Duration;

use serde_json::Value;

const API_URL: &str = "https://api.github.com/repos/uwuclxdy/clauth/releases/latest";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Spawns a background thread that silently checks for and applies an update.
/// Returns immediately. All errors are discarded — update is best-effort.
pub fn spawn() {
    if is_cargo_installed() {
        return;
    }
    std::thread::spawn(|| {
        let _ = try_update();
    });
}

fn try_update() -> anyhow::Result<()> {
    let Some(asset) = asset_name() else {
        return Ok(());
    };

    let api_agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .build();

    let text = api_agent
        .get(API_URL)
        .set("User-Agent", "clauth-updater")
        .call()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .into_string()?;

    let release: Value = serde_json::from_str(&text)?;

    let Some(tag) = release["tag_name"].as_str() else {
        return Ok(());
    };

    if !is_newer(tag, CURRENT_VERSION) {
        return Ok(());
    }

    let Some(url) = asset_url(&release, asset) else {
        return Ok(());
    };

    download_and_replace(&url)
}

fn asset_url(release: &Value, name: &str) -> Option<String> {
    release["assets"]
        .as_array()?
        .iter()
        .find(|a| a["name"].as_str() == Some(name))?["browser_download_url"]
        .as_str()
        .map(str::to_owned)
}

fn download_and_replace(url: &str) -> anyhow::Result<()> {
    let tmp_path = env::temp_dir().join("clauth_update.tmp");

    // Remove any leftover partial file from a previous interrupted attempt.
    let _ = fs::remove_file(&tmp_path);

    let mut bytes = Vec::new();
    // No timeout on the download body — this runs in a background thread.
    ureq::get(url)
        .set("User-Agent", "clauth-updater")
        .call()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .into_reader()
        .read_to_end(&mut bytes)?;

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

    // self_replace handles in-place replacement on all platforms, including
    // Windows where you cannot directly overwrite a running executable.
    self_replace::self_replace(&tmp_path)?;
    let _ = fs::remove_file(&tmp_path);

    Ok(())
}

fn is_cargo_installed() -> bool {
    let Ok(exe) = env::current_exe() else {
        return false;
    };
    let cargo_bin = dirs::home_dir()
        .map(|h| h.join(".cargo").join("bin"))
        .unwrap_or_default();
    exe.starts_with(cargo_bin)
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
