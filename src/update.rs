use std::env;
use std::fs;
use std::io::{Read, Write};
use std::time::Duration;

use serde::Deserialize;

const API_URL: &str = "https://api.github.com/repos/uwuclxdy/clauth/releases/latest";

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

/// Spawns a background thread that silently checks for and applies an update.
/// Returns immediately. All errors are discarded — update is best-effort.
pub(crate) fn spawn() {
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

    let api_agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_recv_response(Some(Duration::from_secs(10)))
        .build()
        .into();

    let text = api_agent
        .get(API_URL)
        .header("User-Agent", "clauth-updater")
        .call()
        .map_err(crate::ureq_error::into_anyhow)?
        .body_mut()
        .read_to_string()
        .map_err(crate::ureq_error::into_anyhow)?;

    let release: Release = serde_json::from_str(&text)?;

    if !is_newer(&release.tag_name, CURRENT_VERSION) {
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

    download_and_replace(&url)
}

fn download_and_replace(url: &str) -> anyhow::Result<()> {
    let tmp_path = env::temp_dir().join("clauth_update.tmp");
    let _ = fs::remove_file(&tmp_path);

    // into_reader() has no size cap, unlike read_to_vec()'s 10 MB default.
    let mut bytes = Vec::new();
    ureq::get(url)
        .header("User-Agent", "clauth-updater")
        .call()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .into_body()
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

    // self_replace handles in-place replacement on every platform, including
    // Windows where you can't directly overwrite a running executable.
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
