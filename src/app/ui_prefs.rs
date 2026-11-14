//! Настройки вида, которые человек меняет на ходу: правая панель и вид главной.
//! Выбор сразу попадает в `[ui]` конфига и переживает перезапуск.

use crate::app::App;
use crate::config::{LandingStyle, PanelMode};

impl App {
    /// Показать, скрыть или вернуть панель на автомат. `None` — переключить видимую.
    pub(crate) fn set_panel(&mut self, mode: Option<PanelMode>) {
        let mode = mode.unwrap_or_else(|| {
            if self.state.stats_panel_visible(self.state.terminal_cols) {
                PanelMode::Hidden
            } else {
                PanelMode::Shown
            }
        });
        self.state.show_stats_panel = mode.forced();
        self.config.ui.panel = mode;
        self.state.status_message = match mode {
            PanelMode::Auto => format!(
                "Side panel: automatic (from {} columns)",
                crate::tui::render::SIDEBAR_AUTO_COLS
            ),
            PanelMode::Shown => "Side panel: shown (Ctrl+B hides it)".to_string(),
            PanelMode::Hidden => "Side panel: hidden (Ctrl+B shows it)".to_string(),
        };
        // Включили, а показать нечем — скажем почему, а не промолчим.
        if mode == PanelMode::Shown
            && let Some(reason) = self.state.stats_panel_blocker()
        {
            self.state.status_message = reason;
        }
        self.save_ui_prefs();
    }

    /// Выбрать вид главной. `None` — переключить на другой.
    pub(crate) fn set_landing(&mut self, style: Option<LandingStyle>) {
        let style = style.unwrap_or(match self.config.ui.landing {
            LandingStyle::Center => LandingStyle::Sheet,
            LandingStyle::Sheet => LandingStyle::Center,
        });
        self.config.ui.landing = style;
        self.state.status_message = format!("Home screen: {}", style.name());
        self.save_ui_prefs();
    }

    /// Запись идёт очередью конфига на рабочем потоке; цикл событий не ждёт диск.
    fn save_ui_prefs(&mut self) {
        if let Err(message) = crate::config::save_or_message(&self.config) {
            self.state.status_message = message;
        }
    }
}
