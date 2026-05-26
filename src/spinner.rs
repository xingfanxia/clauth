//! Stderr spinner for one-shot CLI steps. Silent when stderr is not a
//! terminal so piped/redirected output stays free of control codes. On drop
//! it stops the thread, joins it, and clears its line — so it never outlives
//! the work it wraps or leaks frames into a child's inherited stderr.

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL_MS: u64 = 80;

pub(crate) struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    pub(crate) fn start(message: &str) -> Self {
        if !std::io::stderr().is_terminal() {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let message = message.to_string();
        let handle = std::thread::spawn(move || {
            let mut frame = 0;
            while !stop_thread.load(Ordering::Relaxed) {
                let mut err = std::io::stderr().lock();
                let _ = write!(err, "\r{} {}", SPINNER_FRAMES[frame], message);
                let _ = err.flush();
                drop(err);
                frame = (frame + 1) % SPINNER_FRAMES.len();
                std::thread::sleep(std::time::Duration::from_millis(SPINNER_INTERVAL_MS));
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
            // Clear the spinner line: carriage return + clear-to-EOL so the
            // next stderr/stdout write starts on a clean line.
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
        }
    }
}
