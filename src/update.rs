use std::env;
use std::fs;
use std::io::{Read, Write};
use std::time::Duration;

use serde_json::Value;

const API_URL: &str = "https://api.github.com/repos/uwuclxdy/clauth/releases/latest";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct UpdateInfo {
    pub version: String,
    pub url: String,
}

/// Returns Some if a newer release is available and the current binary is not cargo-managed.
pub fn check() -> Option<UpdateInfo> {
    if is_cargo_installed() {
        return None;
    }

    let asset = asset_name()?;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout_read(Duration::from_secs(5))
        .build();

    let response = agent
        .get(API_URL)
        .set("User-Agent", "clauth-updater")
        .call()
        .ok()?;

    let text = response.into_string().ok()?;
    let release: Value = serde_json::from_str(&text).ok()?;

    let tag = release["tag_name"].as_str()?;
    if !is_newer(tag, CURRENT_VERSION) {
        return None;
    }

    let url = release["assets"]
        .as_array()?
        .iter()
        .find(|a| a["name"].as_str() == Some(asset))?["browser_download_url"]
        .as_str()
        .map(str::to_owned)?;

    Some(UpdateInfo {
        version: tag.to_owned(),
        url,
    })
}

/// Downloads the new binary and replaces the current executable in-place.
pub fn apply(info: &UpdateInfo) {
    eprintln!(
        "Update available: v{} → {}",
        CURRENT_VERSION, info.version
    );
    eprintln!("Downloading...");

    if let Err(e) = download_and_replace(&info.url) {
        eprintln!("Update failed: {e}");
        return;
    }

    eprintln!("Updated to {}. Restarting...", info.version);
    restart();
}

fn download_and_replace(url: &str) -> anyhow::Result<()> {
    let response = ureq::get(url)
        .set("User-Agent", "clauth-updater")
        .call()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let exe_path = env::current_exe()?;
    let exe_name = exe_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clauth".to_owned());
    let tmp_path = exe_path.with_file_name(format!("{exe_name}.new"));

    {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(&bytes)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755))?;
    }

    // Windows cannot rename a running executable, so try and fall back gracefully
    #[cfg(windows)]
    {
        if fs::rename(&tmp_path, &exe_path).is_err() {
            eprintln!(
                "Could not replace running binary. Copy manually:\n  {} → {}",
                tmp_path.display(),
                exe_path.display()
            );
            return Ok(());
        }
    }

    #[cfg(not(windows))]
    fs::rename(&tmp_path, &exe_path)?;

    Ok(())
}

fn restart() -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let exe = env::current_exe().unwrap_or_else(|_| "clauth".into());
        let err = std::process::Command::new(&exe)
            .args(env::args().skip(1))
            .exec();
        eprintln!("Restart failed: {err}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        eprintln!("Restart clauth to use the updated version.");
        std::process::exit(0);
    }
}

fn is_cargo_installed() -> bool {
    let Ok(exe) = env::current_exe() else {
        return false;
    };
    let cargo_bin = dirs::home_dir()
        .map(|h| h.join(".cargo").join("bin"))
        .unwrap_or_default();
    exe.starts_with(&cargo_bin)
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
    let mut parts = v.splitn(3, '.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(latest), Some(cur)) => latest > cur,
        _ => false,
    }
}
