use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::ExecutableCommand;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};

/// Shared shutdown signal between the input-reader background thread
/// in `ui::mod` and `TerminalGuard::drop`. The reader polls this with
/// each `event::poll` tick; the guard sets it before tearing down so
/// the reader exits its loop cooperatively instead of dying mid-read
/// when the process unwinds. Without this flag the reader stays
/// blocked in `event::read()` while the guard's drain pass is also
/// holding crossterm's internal mutex — the two race for terminal-
/// response bytes (OSC 11, primary DA, CPR). Either path consumes
/// them, but the race is real and the outcome is timing-dependent.
pub(crate) static EVENT_READER_SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn new() -> std::io::Result<Self> {
        // Reset the shutdown flag in case the binary previously held a
        // guard in the same process (test harness, embedded use).
        EVENT_READER_SHUTDOWN.store(false, Ordering::Relaxed);
        let mut stdout = std::io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        stdout.execute(Clear(ClearType::All))?;
        stdout.execute(EnableMouseCapture)?;
        // Bracketed paste lets the terminal deliver a multi-line paste as a
        // single Event::Paste, rather than a flood of keystroke events. The
        // input editor relies on this to compress long pastes into a
        // `[N lines pasted]` placeholder.
        stdout.execute(EnableBracketedPaste)?;
        // Hide the hardware cursor by default. While the agent streams output,
        // the renderer issues many MoveTo calls and the visible cursor would
        // flicker across the screen. draw_bottom re-shows it only after
        // positioning it at the input prompt.
        stdout.execute(Hide)?;
        terminal::enable_raw_mode()?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Signal the background event-reader thread to exit its loop.
        // It picks this up at the next `event::poll` tick (up to ~50ms),
        // breaks out of its outer loop, and releases crossterm's
        // internal mutex so our drain below can run without contention.
        EVENT_READER_SHUTDOWN.store(true, Ordering::Relaxed);
        let mut stdout = std::io::stdout();
        // Send the mode-reset sequences (`DisableMouseCapture`,
        // `DisableBracketedPaste`) while we're STILL in raw mode and
        // STILL on the alt screen. Some terminals (and tmux state
        // machines) answer these resets — and other transitions like
        // leaving the alt screen — with synchronous responses (OSC 11
        // bg-color, primary DA `\x1b[?…c`, cursor-position `\x1b[…R`)
        // that travel back through stdin. If raw mode is already
        // disabled when those bytes land, the TTY line discipline
        // echoes them straight to the user's shell prompt instead of
        // letting crossterm parse and discard them.
        let _ = stdout.execute(Show);
        let _ = stdout.execute(DisableBracketedPaste);
        let _ = stdout.execute(DisableMouseCapture);
        let _ = stdout.flush();
        // Give the reader thread a window to observe the flag and
        // exit, then drain anything still in crossterm's queue. A
        // long-ish first poll (60ms) covers the reader's worst-case
        // 50ms poll latency; subsequent passes are short.
        let deadline = std::time::Instant::now() + Duration::from_millis(80);
        let mut first = true;
        loop {
            let wait = if first {
                Duration::from_millis(60)
            } else {
                Duration::from_millis(5)
            };
            first = false;
            match event::poll(wait) {
                Ok(true) => {
                    if event::read().is_err() {
                        break;
                    }
                }
                _ => break,
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        let _ = terminal::disable_raw_mode();
        let _ = stdout.execute(LeaveAlternateScreen);
        let _ = stdout.flush();
    }
}
