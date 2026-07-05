//! State + pure logic for the `/themes` screen: the theme gallery (built-in
//! presets + `[[ui.custom_themes]]` entries, applied instantly on Enter) and
//! the step-by-step create/edit wizard (name → base preset → one step per
//! color role → confirm). Kept as its own module (like `providers.rs` /
//! `mcp_add.rs`) so the flow's state and validation stay cohesive instead of
//! scattered across the key handlers. Everything renders through the *live
//! preview*: while the screen is open, the whole frame is drawn with the
//! highlighted (or in-progress) theme — see `ThemesViewState::preview_theme`.

use crate::app::input::InputState;
use crate::config::{Config, CustomTheme};
use crate::tui::theme::{self, Theme, PRESETS, ROLES};

/// Live state of the `/themes` full-screen panel.
#[derive(Debug, Clone, Default)]
pub struct ThemesViewState {
    pub selected: usize,
    pub status_message: String,
    /// `Some` while the create/edit wizard is open on top of the gallery.
    pub wizard: Option<ThemeWizardState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeRowKind {
    Preset,
    Custom,
    /// The trailing "+ Create your own theme" action row.
    Create,
}

/// One row of the gallery: every preset, then every custom theme, then the
/// create action.
#[derive(Debug, Clone)]
pub struct ThemeRow {
    pub kind: ThemeRowKind,
    /// Config name (`ui.theme` value); empty for the create row.
    pub name: String,
    pub label: String,
    pub detail: String,
    pub active: bool,
}

pub fn theme_rows(config: &Config) -> Vec<ThemeRow> {
    let active = config.ui.theme.as_str();
    let mut rows: Vec<ThemeRow> = PRESETS
        .iter()
        .map(|preset| ThemeRow {
            kind: ThemeRowKind::Preset,
            name: preset.name.to_string(),
            label: preset.label.to_string(),
            detail: preset.description.to_string(),
            active: preset.name == active,
        })
        .collect();
    for custom in &config.ui.custom_themes {
        rows.push(ThemeRow {
            kind: ThemeRowKind::Custom,
            name: custom.name.clone(),
            label: custom.name.clone(),
            detail: format!(
                "custom · based on {}",
                if custom.base.is_empty() { "default" } else { custom.base.as_str() },
            ),
            active: custom.name == active,
        });
    }
    rows.push(ThemeRow {
        kind: ThemeRowKind::Create,
        name: String::new(),
        label: "+ Create your own theme".to_string(),
        detail: "step-by-step color wizard (also: /themes new)".to_string(),
        active: false,
    });
    rows
}

impl ThemesViewState {
    /// Reset to the gallery with the active theme highlighted.
    pub fn open(config: &Config) -> Self {
        Self {
            selected: theme_rows(config)
                .iter()
                .position(|row| row.active)
                .unwrap_or(0),
            status_message: String::new(),
            wizard: None,
        }
    }

    /// The theme the whole frame is drawn with while this screen is open:
    /// the wizard's work-in-progress palette, else the highlighted row's
    /// theme — moving the cursor restyles the entire app instantly, and
    /// leaving without applying restores the saved theme for free (render
    /// re-resolves from config every frame).
    pub fn preview_theme(&self, config: &Config) -> Theme {
        if let Some(wizard) = &self.wizard {
            return wizard.preview_theme();
        }
        let rows = theme_rows(config);
        match rows.get(self.selected) {
            Some(row) if row.kind != ThemeRowKind::Create => {
                Theme::resolve_name(&row.name, &config.ui.custom_themes)
            }
            _ => Theme::resolve(&config.ui),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeWizardStep {
    Name,
    Base,
    /// Index into [`ROLES`].
    Color(usize),
    Confirm,
}

impl ThemeWizardStep {
    pub fn number(self) -> usize {
        match self {
            ThemeWizardStep::Name => 1,
            ThemeWizardStep::Base => 2,
            ThemeWizardStep::Color(index) => 3 + index,
            ThemeWizardStep::Confirm => 3 + ROLES.len(),
        }
    }

    pub fn total() -> usize {
        3 + ROLES.len()
    }

    pub fn label(self) -> String {
        match self {
            ThemeWizardStep::Name => "Theme name".to_string(),
            ThemeWizardStep::Base => "Starting palette".to_string(),
            ThemeWizardStep::Color(index) => ROLES
                .get(index)
                .map(|role| role.label.to_string())
                .unwrap_or_default(),
            ThemeWizardStep::Confirm => "Confirm".to_string(),
        }
    }
}

/// The `/themes new` wizard. Color steps come prefilled from the chosen
/// base palette, so Enter-through is fast: only type where you want to
/// diverge. `colors` holds the accepted hex per role (ROLES order);
/// `hex_input` is the live edit buffer of the current color step.
#[derive(Debug, Clone)]
pub struct ThemeWizardState {
    pub step: ThemeWizardStep,
    pub name: InputState,
    /// Index into [`PRESETS`].
    pub base_selected: usize,
    /// The base whose colors are currently loaded into `colors` — reloading
    /// only on change keeps edits when the user walks back past the Base
    /// step without picking a different palette.
    loaded_base: Option<usize>,
    pub colors: Vec<String>,
    pub hex_input: InputState,
    /// Original name when editing an existing custom theme.
    pub editing: Option<String>,
    pub error: Option<String>,
}

impl ThemeWizardState {
    pub fn new() -> Self {
        Self {
            step: ThemeWizardStep::Name,
            name: InputState::default(),
            base_selected: 0,
            loaded_base: None,
            colors: Vec::new(),
            hex_input: InputState::default(),
            editing: None,
            error: None,
        }
    }

    /// Reopen the wizard over an existing custom theme, everything
    /// prefilled with its current values.
    pub fn edit(custom: &CustomTheme) -> Self {
        let base_selected = PRESETS
            .iter()
            .position(|preset| preset.name == custom.base)
            .unwrap_or(0);
        let resolved = Theme::from_custom(custom);
        let mut wizard = Self::new();
        wizard.name.buffer = custom.name.clone();
        wizard.name.cursor = custom.name.chars().count();
        wizard.base_selected = base_selected;
        wizard.loaded_base = Some(base_selected);
        wizard.colors = colors_of(&resolved);
        wizard.editing = Some(custom.name.clone());
        wizard
    }

    pub fn base_preset(&self) -> &'static theme::ThemePreset {
        &PRESETS[self.base_selected.min(PRESETS.len() - 1)]
    }

    /// Enter `Color(index)`, prefilling the edit buffer with the current
    /// value. Loads the base palette into `colors` first if the base
    /// changed (or was never loaded).
    pub fn enter_color_step(&mut self, index: usize) {
        if self.loaded_base != Some(self.base_selected) {
            self.colors = colors_of(&(self.base_preset().build)());
            self.loaded_base = Some(self.base_selected);
        }
        let value = self.colors.get(index).cloned().unwrap_or_default();
        self.hex_input.buffer = value;
        self.hex_input.cursor = self.hex_input.buffer.chars().count();
        self.step = ThemeWizardStep::Color(index);
        self.error = None;
    }

    /// The work-in-progress palette: base + accepted colors + (on a color
    /// step) the live edit buffer when it already parses — typing a hex
    /// recolors the preview keystroke by keystroke.
    pub fn preview_theme(&self) -> Theme {
        let mut preview = (self.base_preset().build)();
        for (role, hex) in ROLES.iter().zip(&self.colors) {
            if let Some(color) = theme::parse_hex(hex) {
                preview.set(role.key, color);
            }
        }
        if let ThemeWizardStep::Color(index) = self.step
            && let Some(role) = ROLES.get(index)
            && let Some(color) = theme::parse_hex(&self.hex_input.buffer)
        {
            preview.set(role.key, color);
        }
        preview
    }

    /// Assemble the final config entry. Steps validate on advance, so a
    /// failure here means the two validations disagree — kept fallible
    /// rather than assumed-infallible (same stance as the other wizards).
    pub fn build_custom(&self, config: &Config) -> Result<CustomTheme, String> {
        let name = self.name.buffer.trim().to_string();
        validate_theme_name(&name, config, self.editing.as_deref())?;
        let colors = ROLES
            .iter()
            .zip(&self.colors)
            .map(|(role, hex)| Ok((role.key.to_string(), validate_hex(hex)?)))
            .collect::<Result<_, String>>()?;
        Ok(CustomTheme {
            name,
            base: self.base_preset().name.to_string(),
            colors,
        })
    }
}

/// `colors` vector (ROLES order) for a resolved theme.
pub fn colors_of(theme: &Theme) -> Vec<String> {
    ROLES
        .iter()
        .map(|role| theme::to_hex(theme.get(role.key).unwrap_or_default()))
        .collect()
}

/// A usable custom-theme name: non-empty, not shadowing a preset, unique
/// among custom themes (except the one being edited).
pub fn validate_theme_name(
    name: &str,
    config: &Config,
    editing: Option<&str>,
) -> Result<(), String> {
    if name.is_empty() {
        return Err("name can't be empty".to_string());
    }
    if PRESETS.iter().any(|preset| preset.name == name) {
        return Err(format!("'{name}' is a built-in theme"));
    }
    if editing != Some(name)
        && config.ui.custom_themes.iter().any(|theme| theme.name == name)
    {
        return Err(format!("'{name}' already exists"));
    }
    Ok(())
}

/// Normalized `#rrggbb`, or a human-readable error.
pub fn validate_hex(text: &str) -> Result<String, String> {
    theme::parse_hex(text)
        .map(theme::to_hex)
        .ok_or_else(|| format!("'{}' is not a #rrggbb color", text.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn config_with_custom(name: &str) -> Config {
        let mut config = Config::default();
        config.ui.custom_themes.push(CustomTheme {
            name: name.to_string(),
            base: "nord".to_string(),
            colors: BTreeMap::new(),
        });
        config
    }

    #[test]
    fn rows_list_presets_customs_then_create_and_mark_active() {
        let mut config = config_with_custom("mine");
        config.ui.theme = "mine".to_string();
        let rows = theme_rows(&config);
        assert_eq!(rows.len(), PRESETS.len() + 2);
        assert!(rows[..PRESETS.len()].iter().all(|row| row.kind == ThemeRowKind::Preset));
        assert_eq!(rows[PRESETS.len()].kind, ThemeRowKind::Custom);
        assert!(rows[PRESETS.len()].active);
        assert_eq!(rows.last().unwrap().kind, ThemeRowKind::Create);
        assert_eq!(rows.iter().filter(|row| row.active).count(), 1);
    }

    #[test]
    fn open_selects_the_active_theme() {
        let mut config = Config::default();
        config.ui.theme = "nord".to_string();
        let view = ThemesViewState::open(&config);
        assert_eq!(theme_rows(&config)[view.selected].name, "nord");
        // Unknown active theme falls back to the first row.
        config.ui.theme = "gone".to_string();
        assert_eq!(ThemesViewState::open(&config).selected, 0);
    }

    #[test]
    fn preview_follows_the_highlighted_row() {
        let config = Config::default();
        let mut view = ThemesViewState::open(&config);
        let dracula_index = theme_rows(&config)
            .iter()
            .position(|row| row.name == "dracula")
            .unwrap();
        view.selected = dracula_index;
        assert_eq!(view.preview_theme(&config), Theme::preset("dracula").unwrap());
        // The create row previews the currently configured theme.
        view.selected = theme_rows(&config).len() - 1;
        assert_eq!(view.preview_theme(&config), Theme::resolve(&config.ui));
    }

    #[test]
    fn wizard_step_numbers_are_contiguous() {
        assert_eq!(ThemeWizardStep::Name.number(), 1);
        assert_eq!(ThemeWizardStep::Base.number(), 2);
        assert_eq!(ThemeWizardStep::Color(0).number(), 3);
        assert_eq!(
            ThemeWizardStep::Color(ROLES.len() - 1).number() + 1,
            ThemeWizardStep::Confirm.number(),
        );
        assert_eq!(ThemeWizardStep::Confirm.number(), ThemeWizardStep::total());
    }

    #[test]
    fn entering_a_color_step_loads_base_once_and_keeps_edits() {
        let mut wizard = ThemeWizardState::new();
        wizard.base_selected = PRESETS.iter().position(|p| p.name == "nord").unwrap();
        wizard.enter_color_step(0);
        assert_eq!(wizard.colors.len(), ROLES.len());
        assert_eq!(wizard.hex_input.buffer, wizard.colors[0]);

        // An accepted edit survives re-entering steps with the same base…
        wizard.colors[0] = "#010203".to_string();
        wizard.enter_color_step(0);
        assert_eq!(wizard.hex_input.buffer, "#010203");

        // …but switching the base reloads the palette.
        wizard.base_selected = 0;
        wizard.enter_color_step(0);
        assert_eq!(wizard.colors, colors_of(&(PRESETS[0].build)()));
    }

    #[test]
    fn preview_applies_live_hex_input_when_valid() {
        let mut wizard = ThemeWizardState::new();
        let accent_index = ROLES.iter().position(|role| role.key == "accent").unwrap();
        wizard.enter_color_step(accent_index);
        wizard.hex_input.buffer = "#123456".to_string();
        assert_eq!(
            wizard.preview_theme().accent,
            ratatui::style::Color::Rgb(0x12, 0x34, 0x56),
        );
        // A half-typed value leaves the accepted color in place.
        wizard.hex_input.buffer = "#123".to_string();
        assert_eq!(
            wizard.preview_theme().accent,
            (PRESETS[0].build)().accent,
        );
    }

    #[test]
    fn build_custom_round_trips_and_validates() {
        let mut wizard = ThemeWizardState::new();
        wizard.name.buffer = "sunset".to_string();
        wizard.enter_color_step(0);
        wizard.colors[0] = "#0A0B0C".to_string();

        let built = wizard.build_custom(&Config::default()).unwrap();
        assert_eq!(built.name, "sunset");
        assert_eq!(built.base, "default");
        assert_eq!(built.colors.get("bg").map(String::as_str), Some("#0a0b0c"));
        assert_eq!(built.colors.len(), ROLES.len());

        // The resolved theme honors the override.
        assert_eq!(
            Theme::from_custom(&built).bg,
            ratatui::style::Color::Rgb(0x0a, 0x0b, 0x0c),
        );
    }

    #[test]
    fn theme_names_reject_presets_and_duplicates_but_allow_editing_self() {
        let config = config_with_custom("mine");
        assert!(validate_theme_name("", &config, None).is_err());
        assert!(validate_theme_name("dracula", &config, None).is_err());
        assert!(validate_theme_name("mine", &config, None).is_err());
        assert!(validate_theme_name("mine", &config, Some("mine")).is_ok());
        assert!(validate_theme_name("fresh", &config, None).is_ok());
    }

    #[test]
    fn edit_prefills_from_the_existing_theme() {
        let custom = CustomTheme {
            name: "mine".to_string(),
            base: "gruvbox".to_string(),
            colors: BTreeMap::from([("accent".to_string(), "#c0ffee".to_string())]),
        };
        let wizard = ThemeWizardState::edit(&custom);
        assert_eq!(wizard.name.buffer, "mine");
        assert_eq!(wizard.base_preset().name, "gruvbox");
        assert_eq!(wizard.editing.as_deref(), Some("mine"));
        let accent_index = ROLES.iter().position(|role| role.key == "accent").unwrap();
        assert_eq!(wizard.colors[accent_index], "#c0ffee");
    }
}
