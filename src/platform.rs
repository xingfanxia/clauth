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
