use crate::app::AppState;
use crate::config::Config;
use crate::tui::TuiTerminal;
use crate::tui::theme::Theme;
use crate::tui::widgets;
use ratatui::layout::{Constraint, Direction, Layout};

pub fn render(terminal: &mut TuiTerminal, state: &AppState, config: &Config) -> crate::error::AppResult<()> {
    let theme = Theme::default_dark();
    terminal.draw(|frame| {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),    // Chat area
                Constraint::Length(3), // Input area
                Constraint::Length(1), // Status bar
            ])
            .split(area);

        // Chat messages
        widgets::chat::render_chat(frame, chunks[0], state, &theme);

        // Input box
        widgets::input::render_input(frame, chunks[1], state, &theme);

        // Status bar
        widgets::status::render_status(frame, chunks[2], state, config, &theme);
    })?;
    Ok(())
}
