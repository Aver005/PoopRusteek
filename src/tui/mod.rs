pub mod markdown;
pub mod render;
pub mod theme;
pub mod view_model;
pub mod widgets;

use color_eyre::Result;
use crossterm::{
    cursor::{DisableBlinking, EnableBlinking, Show},
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub type TuiTerminal = Terminal<CrosstermBackend<std::io::Stdout>>;

pub fn init() -> Result<TuiTerminal> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // EnableMouseCapture routes wheel/click events to the app so the wheel can
    // scroll the message window. Trade-off: the terminal's own text selection
    // now needs the Shift modifier (standard for full-screen TUIs).
    execute!(
        stdout,
        EnterAlternateScreen,
        DisableBlinking,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.show_cursor()?;
    Ok(terminal)
}

pub fn restore(terminal: &mut TuiTerminal) -> Result<()> {
    terminal.show_cursor()?;
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        Show,
        EnableBlinking,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    Ok(())
}
