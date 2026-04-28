/// Enable virtual-terminal processing on Windows so raw ANSI escape codes
/// in menu labels render correctly even before inquire takes over the terminal.
/// No-op on macOS and Linux where ANSI is always supported.
pub(crate) fn init() {
    #[cfg(windows)]
    {
        use crossterm::execute;
        use std::io::stdout;
        // A no-op command is enough to trigger crossterm's ANSI-enable path.
        let _ = execute!(stdout(), crossterm::style::ResetColor);
    }
}
