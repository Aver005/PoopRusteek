#[cfg(test)]
mod capture;
#[cfg(test)]
mod capture_accounts;
pub mod glyphs;
pub mod markdown;
pub mod render;
pub mod theme;
pub mod view_model;
pub mod widgets;
pub mod wrap;

use color_eyre::Result;
use crossterm::{
    cursor::{DisableBlinking, EnableBlinking, Show},
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::sync::{Once, OnceLock};
use std::thread::ThreadId;

pub type TuiTerminal = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Поток, в котором крутится интерфейс. Паника в нём фатальна для экрана,
/// а паника хода агента на рабочем потоке перехватывается — терминал ей трогать нельзя.
static UI_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// Паника в потоке интерфейса оставила бы терминал в raw mode и альтернативном экране.
fn panic_needs_restore(ui_thread: Option<ThreadId>, current: ThreadId) -> bool {
    ui_thread == Some(current)
}

/// Выключить всё, что включил `init`. Работает с любым выводом, поэтому годится и для хука паники.
fn leave_terminal_modes(out: &mut impl std::io::Write) -> std::io::Result<()> {
    disable_raw_mode()?;
    execute!(
        out,
        Show,
        EnableBlinking,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
}

/// Хук ставится после `color_eyre::install()` и зовёт его хук следующим:
/// отчёт об ошибке печатается уже на обычном экране, а не поверх интерфейса.
fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if panic_needs_restore(UI_THREAD.get().copied(), std::thread::current().id()) {
                let _ = leave_terminal_modes(&mut std::io::stdout());
            }
            previous(info);
        }));
    });
}

pub fn init() -> Result<TuiTerminal> {
    let _ = UI_THREAD.set(std::thread::current().id());
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // EnableMouseCapture routes wheel/click events to the app so the wheel can
    // scroll the message window. Trade-off: the terminal's own text selection
    // now needs the Shift modifier (standard for full-screen TUIs).
    // EnableBracketedPaste makes the terminal deliver a paste as one
    // `Event::Paste(String)` instead of a stream of key events, so multi-line
    // pastes land in the input buffer intact instead of firing an early submit
    // on the first embedded newline (see `keys::paste`).
    execute!(
        stdout,
        EnterAlternateScreen,
        DisableBlinking,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.show_cursor()?;
    Ok(terminal)
}

pub fn restore(terminal: &mut TuiTerminal) -> Result<()> {
    terminal.show_cursor()?;
    leave_terminal_modes(terminal.backend_mut())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_panic_on_the_ui_thread_restores_the_terminal() {
        let ui = std::thread::current().id();
        let other = std::thread::spawn(|| std::thread::current().id())
            .join()
            .expect("thread");
        assert!(panic_needs_restore(Some(ui), ui));
        assert!(!panic_needs_restore(Some(ui), other));
        assert!(!panic_needs_restore(None, ui));
    }
}
