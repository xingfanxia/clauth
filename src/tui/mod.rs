//! TUI runtime. `run` is the only public surface; everything below is glue
//! between ratatui, the `App` state machine, and shutdown housekeeping.

mod app;
mod render;
mod theme;

use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::profile::AppConfig;

/// 100ms tick: snappy enough for blink animation, cheap enough that the input
/// thread stays responsive without burning CPU on idle redraws.
const TICK: Duration = Duration::from_millis(100);

/// Launch the full-screen TUI against a loaded config. Returns when the user
/// quits via q / ⎋ / Ctrl+C, or after a fatal error.
pub(crate) fn run(config: AppConfig) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let outcome = run_loop(&mut terminal, config);
    let restore = restore_terminal(&mut terminal);
    outcome.and(restore)
}

type Term = Terminal<CrosstermBackend<io::Stdout>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("Failed to enter alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("Failed to construct ratatui terminal")
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    Ok(())
}

fn run_loop(terminal: &mut Term, config: AppConfig) -> Result<()> {
    let mut application = app::App::new(config);
    // Kick off startup reconciliation. This is non-blocking: the network-free
    // decision runs inline, and the HTTP refresh probe (only needed when the
    // live credentials diverge) is spawned onto a worker. Its verdict — and,
    // crucially, the gate that token rotation must not race a soon-to-be-
    // disowned profile — is sequenced through `StartupSignal`, drained in
    // `on_tick`. The bootstrap (relink, refresh-all, initial fetch, kick) is
    // likewise spawned from `on_tick` once reconcile settles, so neither runs
    // before the first paint below.
    app::reconcile_startup(&mut application);

    let mut last_tick = Instant::now();

    while !application.quit {
        terminal.draw(|frame| render::draw(frame, &application))?;

        let timeout = TICK.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app::handle_key(&mut application, key);
                }
                Event::Resize(_, _) => { /* redraw next iteration */ }
                _ => {}
            }
        }

        if last_tick.elapsed() >= TICK {
            app::on_tick(&mut application);
            last_tick = Instant::now();
        }
    }

    app::shutdown(&mut application)
}
