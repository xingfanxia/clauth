/// Enable VT processing on Windows for ANSI escapes in raw mode. No-op on
/// macOS/Linux.
pub(crate) fn init() {
    #[cfg(windows)]
    {
        use ratatui::crossterm::execute;
        use std::io::stdout;
        // A no-op command triggers crossterm's ANSI-enable path.
        let _ = execute!(stdout(), ratatui::crossterm::style::ResetColor);
    }
}

/// Open `url` in the operator's default browser. Used by the interactive OAuth
/// login (`oauth_login`) to launch the authorize page. Detached (stdio nulled)
/// so it never blocks or leaks output into clauth's own stdout/stderr.
pub(crate) fn open_url(url: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::process::{Command, Stdio};

    #[cfg(target_os = "macos")]
    let mut cmd = Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = Command::new("xdg-open");
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        // Empty title arg so a quoted URL isn't consumed as the window title.
        c.args(["/C", "start", ""]);
        c
    };

    cmd.arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to open browser for {url}"))?;
    Ok(())
}
