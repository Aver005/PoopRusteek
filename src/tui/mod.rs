pub mod render;
pub mod widgets;
pub mod theme;
pub mod markdown;
pub mod view_model;

use color_eyre::Result;
use crossterm::{
    execute,
    cursor::{DisableBlinking, EnableBlinking, Show},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

pub type TuiTerminal = Terminal<CrosstermBackend<std::io::Stdout>>;

pub fn init() -> Result<TuiTerminal> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, DisableBlinking)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.show_cursor()?;
    Ok(terminal)
}

pub fn restore(terminal: &mut TuiTerminal) -> Result<()> {
    terminal.show_cursor()?;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), Show, EnableBlinking, LeaveAlternateScreen)?;
    Ok(())
}
