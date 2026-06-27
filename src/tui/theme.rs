use ratatui::style::Color;

pub struct Theme {
    pub bg: Color,
    pub panel: Color,
    pub panel_alt: Color,
    pub fg: Color,
    pub accent: Color,
    pub accent_dim: Color,
    pub accent_soft: Color,
    pub border: Color,
    pub border_focus: Color,
    pub text_dim: Color,
    pub text_soft: Color,
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
            bg: Color::Rgb(11, 14, 25),
            panel: Color::Rgb(17, 23, 39),
            panel_alt: Color::Rgb(22, 30, 50),
            fg: Color::Rgb(226, 232, 240),
            accent: Color::Rgb(96, 165, 250),
            accent_dim: Color::Rgb(59, 130, 246),
            accent_soft: Color::Rgb(125, 211, 252),
            border: Color::Rgb(42, 56, 84),
            border_focus: Color::Rgb(96, 165, 250),
            text_dim: Color::Rgb(120, 136, 164),
            text_soft: Color::Rgb(148, 163, 184),
            error: Color::Rgb(243, 139, 168),
            success: Color::Rgb(166, 227, 161),
            warning: Color::Rgb(249, 226, 175),
            user_bg: Color::Rgb(28, 39, 64),
            assistant_bg: Color::Rgb(17, 23, 39),
            tool_bg: Color::Rgb(38, 32, 58),
            status_bg: Color::Rgb(12, 18, 32),
            input_bg: Color::Rgb(15, 21, 36),
            selection: Color::Rgb(34, 45, 72),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::default_dark()
    }
}
