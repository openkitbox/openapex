use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// Whether this process turned mouse capture on, so `restore` (also run by the
/// panic hook) only undoes what `new` did.
static MOUSE_CAPTURED: AtomicBool = AtomicBool::new(false);

pub struct TerminalGuard {
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// `mouse` off leaves the terminal's native text selection in charge.
    pub fn new(mouse: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            // The prompt no longer paints a blinking block into the buffer; the
            // terminal's own cursor is the caret now (an IME anchors on it), so
            // ask for the blinking block the old fake cursor imitated.
            SetCursorStyle::BlinkingBlock
        )?;
        if mouse {
            execute!(io::stdout(), EnableMouseCapture)?;
            MOUSE_CAPTURED.store(true, Ordering::Relaxed);
        }
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok(Self { terminal })
    }
}

/// The single way back to the user's terminal, shared by `Drop` and the panic
/// hook.
fn restore() {
    let _ = disable_raw_mode();
    if MOUSE_CAPTURED.swap(false, Ordering::Relaxed) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        // A panic inside a draw leaves the frame's synchronized update open.
        EndSynchronizedUpdate,
        SetCursorStyle::DefaultUserShape,
        Show
    );
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Install after creating the guard: std runs panic hooks before unwinding,
/// so the screen must be restored first or the panic message disappears.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}
