// src/main.rs – Entry point: terminal setup + event loop.

mod app;
mod api;
mod config;
mod downloader;
mod types;
mod ui;

use std::io;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::App;
use types::AppEvent;

// ── Terminal setup / teardown ─────────────────────────────────────────────

fn setup_terminal() -> anyhow::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

// ── Async main ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut terminal = setup_terminal()?;

    let result = run(&mut terminal).await;

    restore_terminal(&mut terminal)?;

    if let Err(ref e) = result {
        eprintln!("udemyget error: {e}");
    }
    result
}

// ── Event loop ────────────────────────────────────────────────────────────

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> anyhow::Result<()> {
    // Unbounded channel: background tasks → main loop
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();

    let mut app = App::new();

    // Read cookies.txt and kick off verification immediately.
    // If the file is missing/empty this is a no-op and the setup screen shows.
    app.auto_init(tx.clone());

    loop {
        // ── Draw ──────────────────────────────────────────────────────────
        terminal.draw(|f| ui::render(f, &app))?;

        // ── Check quit flag ───────────────────────────────────────────────
        if app.should_quit {
            break;
        }

        // ── Terminal events (non-blocking, 50 ms poll) ────────────────────
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                // Only act on key-press events, not key-release/repeat.
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if app.handle_key(key, tx.clone()) {
                        break; // Ctrl-C
                    }
                }
                Event::Resize(_, _) | Event::Key(_) => {}
                _ => {}
            }
        }

        // ── Background events ─────────────────────────────────────────────
        // Drain all pending events (non-blocking)
        loop {
            match rx.try_recv() {
                Ok(event) => {
                    app.handle_event(event);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return Err(anyhow::anyhow!("event channel disconnected"));
                }
            }
        }
    }

    Ok(())
}
