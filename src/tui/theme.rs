use ratatui::style::{Color, Modifier, Style};

pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub accent: Color,
    pub accent_dim: Color,
    pub border: Color,
    pub border_focus: Color,
    pub text_dim: Color,
    pub error: Color,
    pub success: Color,
    pub warning: Color,
    pub user_bg: Color,
    pub assistant_bg: Color,
    pub tool_bg: Color,
    pub status_bg: Color,
    pub input_bg: Color,
    pub selection: Color,
}

impl Theme {
    pub fn default_dark() -> Self {
        Self {
            bg: Color::Rgb(30, 30, 46),
            fg: Color::Rgb(205, 214, 244),
            accent: Color::Rgb(137, 180, 250),
            accent_dim: Color::Rgb(88, 113, 171),
            border: Color::Rgb(69, 71, 90),
            border_focus: Color::Rgb(137, 180, 250),
            text_dim: Color::Rgb(108, 112, 134),
            error: Color::Rgb(243, 139, 168),
            success: Color::Rgb(166, 227, 161),
            warning: Color::Rgb(249, 226, 175),
            user_bg: Color::Rgb(49, 50, 68),
            assistant_bg: Color::Rgb(30, 30, 46),
            tool_bg: Color::Rgb(39, 39, 55),
            status_bg: Color::Rgb(24, 24, 37),
            input_bg: Color::Rgb(39, 39, 55),
            selection: Color::Rgb(88, 91, 112),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::default_dark()
    }
}

impl Theme {
    pub fn border_style(&self, focused: bool) -> Style {
        Style::default().fg(if focused { self.border_focus } else { self.border })
    }

    pub fn text_style(&self) -> Style {
        Style::default().fg(self.fg).bg(self.bg)
    }

    pub fn dim_style(&self) -> Style {
        Style::default().fg(self.text_dim).bg(self.bg)
    }

    pub fn accent_style(&self) -> Style {
        Style::default().fg(self.accent).bg(self.bg)
    }

    pub fn bold_accent_style(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .bg(self.bg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn error_style(&self) -> Style {
        Style::default().fg(self.error).bg(self.bg)
    }

    pub fn success_style(&self) -> Style {
        Style::default().fg(self.success).bg(self.bg)
    }
}
