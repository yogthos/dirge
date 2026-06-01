//! Terminal input reader. Split out of `ui/mod.rs::run_interactive`
//! (dirge-4y4l stage 12a): a dedicated OS thread that polls crossterm for
//! key/mouse/paste/resize events and forwards them to the UI loop as
//! [`UserEvent`]s over an mpsc channel. Kept off the async runtime because
//! `event::read()` is blocking; cooperative shutdown via the terminal
//! module's `EVENT_READER_SHUTDOWN` / `EVENT_READER_EXITED` flags.

use crossterm::event;
use crossterm::event::{MouseButton, MouseEventKind};

use crate::event::UserEvent;

/// Spawn the blocking crossterm reader thread. `user_tx` is consumed (pass
/// a clone — the caller keeps its own sender for other event sources). The
/// `JoinHandle` is intentionally dropped: the thread exits on its own when
/// the channel closes or `EVENT_READER_SHUTDOWN` is set.
pub(super) fn spawn_input_reader(user_tx: tokio::sync::mpsc::Sender<UserEvent>) {
    std::thread::spawn(move || {
        // Poll-based loop so `TerminalGuard::drop` can signal a
        // cooperative shutdown via `EVENT_READER_SHUTDOWN`. Previously
        // this thread blocked in `event::read()` indefinitely; on
        // teardown the guard's drain pass and this `read()` both held
        // crossterm's internal mutex, racing for terminal-response
        // bytes (OSC 11, primary DA, CPR). With the flag + 50ms
        // poll-tick, the reader exits within ~50ms of the guard
        // signalling, the mutex is released, and the drain runs
        // uncontended.
        loop {
            if crate::ui::terminal::EVENT_READER_SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed)
            {
                break;
            }
            match event::poll(std::time::Duration::from_millis(50)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            // `clippy::collapsible_match` suggests moving the `is_err()` check into
            // a match guard, but doing so tries to move bound values (e.g. `text`
            // in `Event::Paste(text)`) inside the guard, which is rejected with
            // E0507. Keep the nested `if`s.
            #[allow(clippy::collapsible_match)]
            match event::read() {
                Ok(event::Event::Key(key)) => {
                    // Filter Release / Repeat events. Modern terminals
                    // (kitty keyboard protocol, Windows 10+ ConPTY,
                    // some iTerm2 modes) emit BOTH Press and Release
                    // for every keystroke — without this filter every
                    // typed char inserts twice ("ssuubb..." bug).
                    if key.kind != event::KeyEventKind::Press {
                        continue;
                    }
                    if user_tx.blocking_send(UserEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(event::Event::Mouse(m)) => {
                    // Wheel → scroll the output pane. Left button
                    // down/drag/up → app-level text selection
                    // (`ui::selection::handle`). Other buttons are
                    // ignored. Right/middle clicks fall through with
                    // no app action and the terminal's own handling
                    // for them takes over (paste, menu, etc.).
                    let ev = match m.kind {
                        MouseEventKind::ScrollUp => Some(UserEvent::ScrollUp {
                            row: m.row,
                            col: m.column,
                        }),
                        MouseEventKind::ScrollDown => Some(UserEvent::ScrollDown {
                            row: m.row,
                            col: m.column,
                        }),
                        MouseEventKind::Down(MouseButton::Left) => Some(UserEvent::MouseDown {
                            row: m.row,
                            col: m.column,
                        }),
                        MouseEventKind::Drag(MouseButton::Left) => Some(UserEvent::MouseDrag {
                            row: m.row,
                            col: m.column,
                        }),
                        MouseEventKind::Up(MouseButton::Left) => Some(UserEvent::MouseUp {
                            row: m.row,
                            col: m.column,
                        }),
                        _ => None,
                    };
                    if let Some(ev) = ev
                        && user_tx.blocking_send(ev).is_err()
                    {
                        break;
                    }
                }
                Ok(event::Event::Paste(text)) => {
                    if user_tx.blocking_send(UserEvent::Paste(text)).is_err() {
                        break;
                    }
                }
                Ok(event::Event::Resize(_, _)) => {
                    if user_tx.blocking_send(UserEvent::Resize).is_err() {
                        break;
                    }
                }
                Err(_) => break,
                _ => {}
            }
        }
        // Tell `TerminalGuard::drop` we've actually exited so it can
        // proceed past the wait barrier without sleeping on a
        // timeout. Release-store paired with the guard's
        // Acquire-load gives a clean happens-before relationship —
        // by the time the guard observes `true`, every byte this
        // thread consumed from crossterm's internal buffer is
        // visible to subsequent reads.
        crate::ui::terminal::EVENT_READER_EXITED.store(true, std::sync::atomic::Ordering::Release);
    });
}
