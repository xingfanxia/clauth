/// Enable virtual-terminal processing on Windows so ANSI escapes in raw mode
/// render correctly even before ratatui's terminal takes over. No-op on
/// macOS and Linux where ANSI is always supported.
pub(crate) fn init() {
    #[cfg(windows)]
    {
        use ratatui::crossterm::execute;
        use std::io::stdout;
        // A no-op command is enough to trigger crossterm's ANSI-enable path.
        let _ = execute!(stdout(), ratatui::crossterm::style::ResetColor);
    }
}
