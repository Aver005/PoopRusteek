//! The color system: a [`Theme`] of semantic roles every widget draws
//! from (never hardcode RGB in widgets), a catalog of built-in presets
//! ([`PRESETS`]), and resolution of the active theme from `[ui]` config —
//! preset name or a `[[ui.custom_themes]]` entry (base preset + per-role
//! hex overrides, built by the `/themes` wizard).

use crate::config::{CustomTheme, UiConfig};
use ratatui::style::Color;

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub bg: Color,
    pub panel: Color,
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
    pub tool_bg: Color,
    pub input_bg: Color,
    pub selection: Color,
}

/// `0xRRGGBB` → `Color::Rgb` — keeps the preset tables readable.
const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// One semantic slot of a [`Theme`], addressable by a stable string key —
/// the contract between preset palettes, `[[ui.custom_themes]]` color maps
/// on disk, and the `/themes` wizard's step list.
pub struct ThemeRole {
    pub key: &'static str,
    pub label: &'static str,
}

/// Every role, in `Theme` field order — the wizard walks this list.
pub const ROLES: &[ThemeRole] = &[
    ThemeRole { key: "bg", label: "App background" },
    ThemeRole { key: "panel", label: "Panel / popup background" },
    ThemeRole { key: "fg", label: "Primary text" },
    ThemeRole { key: "accent", label: "Accent (titles, focused elements)" },
    ThemeRole { key: "accent_dim", label: "Accent dim (selected row background)" },
    ThemeRole { key: "accent_soft", label: "Accent soft (tags, secondary highlights)" },
    ThemeRole { key: "border", label: "Borders" },
    ThemeRole { key: "border_focus", label: "Focused border" },
    ThemeRole { key: "text_dim", label: "Dim text (hints, metadata)" },
    ThemeRole { key: "text_soft", label: "Soft text (secondary content)" },
    ThemeRole { key: "error", label: "Errors" },
    ThemeRole { key: "success", label: "Success" },
    ThemeRole { key: "warning", label: "Warnings" },
    ThemeRole { key: "user_bg", label: "User message background" },
    ThemeRole { key: "tool_bg", label: "Tool message background" },
    ThemeRole { key: "input_bg", label: "Input box background" },
    ThemeRole { key: "selection", label: "Selection background" },
];

/// A built-in theme: stable config name, display label, one-line pitch.
pub struct ThemePreset {
    pub name: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub build: fn() -> Theme,
}

pub const PRESETS: &[ThemePreset] = &[
    ThemePreset {
        name: "default",
        label: "Midnight",
        description: "the original deep-navy look with electric blue accents",
        build: Theme::default_dark,
    },
    ThemePreset {
        name: "catppuccin",
        label: "Catppuccin Mocha",
        description: "soothing pastels on a warm dark base, mauve accents",
        build: Theme::catppuccin_mocha,
    },
    ThemePreset {
        name: "dracula",
        label: "Dracula",
        description: "the classic purple-and-pink vampire palette",
        build: Theme::dracula,
    },
    ThemePreset {
        name: "gruvbox",
        label: "Gruvbox Dark",
        description: "retro warm earth tones with punchy orange",
        build: Theme::gruvbox_dark,
    },
    ThemePreset {
        name: "nord",
        label: "Nord",
        description: "arctic blues and frosted polar-night calm",
        build: Theme::nord,
    },
    ThemePreset {
        name: "tokyo-night",
        label: "Tokyo Night",
        description: "neon-lit indigo inspired by downtown Tokyo at night",
        build: Theme::tokyo_night,
    },
    ThemePreset {
        name: "rose-pine",
        label: "Rosé Pine",
        description: "muted florals — iris, rose and gold on soot",
        build: Theme::rose_pine,
    },
    ThemePreset {
        name: "synthwave",
        label: "Synthwave '84",
        description: "hot pink and cyan straight off a retro-future VHS",
        build: Theme::synthwave,
    },
    ThemePreset {
        name: "matrix",
        label: "Matrix",
        description: "phosphor green rain on pitch black",
        build: Theme::matrix,
    },
    ThemePreset {
        name: "paper",
        label: "Paper Light",
        description: "a light theme — warm paper white with indigo ink",
        build: Theme::paper_light,
    },
];

impl Theme {
    pub fn default_dark() -> Self {
        Self {
            bg: Color::Rgb(11, 14, 25),
            panel: Color::Rgb(17, 23, 39),
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
            tool_bg: Color::Rgb(38, 32, 58),
            input_bg: Color::Rgb(15, 21, 36),
            selection: Color::Rgb(34, 45, 72),
        }
    }

    fn catppuccin_mocha() -> Self {
        Self {
            bg: rgb(0x1e1e2e),
            panel: rgb(0x181825),
            fg: rgb(0xcdd6f4),
            accent: rgb(0xcba6f7),
            accent_dim: rgb(0x6c5a99),
            accent_soft: rgb(0xb4befe),
            border: rgb(0x313244),
            border_focus: rgb(0xcba6f7),
            text_dim: rgb(0x6c7086),
            text_soft: rgb(0xa6adc8),
            error: rgb(0xf38ba8),
            success: rgb(0xa6e3a1),
            warning: rgb(0xf9e2af),
            user_bg: rgb(0x313244),
            tool_bg: rgb(0x2a2438),
            input_bg: rgb(0x181825),
            selection: rgb(0x45475a),
        }
    }

    fn dracula() -> Self {
        Self {
            bg: rgb(0x282a36),
            panel: rgb(0x21222c),
            fg: rgb(0xf8f8f2),
            accent: rgb(0xbd93f9),
            accent_dim: rgb(0x62479c),
            accent_soft: rgb(0xff79c6),
            border: rgb(0x44475a),
            border_focus: rgb(0xbd93f9),
            text_dim: rgb(0x6272a4),
            text_soft: rgb(0x9ea8c7),
            error: rgb(0xff5555),
            success: rgb(0x50fa7b),
            warning: rgb(0xf1fa8c),
            user_bg: rgb(0x3a3d4d),
            tool_bg: rgb(0x35304a),
            input_bg: rgb(0x21222c),
            selection: rgb(0x44475a),
        }
    }

    fn gruvbox_dark() -> Self {
        Self {
            bg: rgb(0x282828),
            panel: rgb(0x1d2021),
            fg: rgb(0xebdbb2),
            accent: rgb(0xfe8019),
            accent_dim: rgb(0x9d5300),
            accent_soft: rgb(0xfabd2f),
            border: rgb(0x3c3836),
            border_focus: rgb(0xfe8019),
            text_dim: rgb(0x7c6f64),
            text_soft: rgb(0xa89984),
            error: rgb(0xfb4934),
            success: rgb(0xb8bb26),
            warning: rgb(0xfabd2f),
            user_bg: rgb(0x3c3836),
            tool_bg: rgb(0x32302f),
            input_bg: rgb(0x1d2021),
            selection: rgb(0x504945),
        }
    }

    fn nord() -> Self {
        Self {
            bg: rgb(0x2e3440),
            panel: rgb(0x292e39),
            fg: rgb(0xeceff4),
            accent: rgb(0x88c0d0),
            accent_dim: rgb(0x5e81ac),
            accent_soft: rgb(0x8fbcbb),
            border: rgb(0x3b4252),
            border_focus: rgb(0x88c0d0),
            text_dim: rgb(0x616e88),
            text_soft: rgb(0xd8dee9),
            error: rgb(0xbf616a),
            success: rgb(0xa3be8c),
            warning: rgb(0xebcb8b),
            user_bg: rgb(0x3b4252),
            tool_bg: rgb(0x434c5e),
            input_bg: rgb(0x292e39),
            selection: rgb(0x434c5e),
        }
    }

    fn tokyo_night() -> Self {
        Self {
            bg: rgb(0x1a1b26),
            panel: rgb(0x16161e),
            fg: rgb(0xc0caf5),
            accent: rgb(0x7aa2f7),
            accent_dim: rgb(0x3d59a1),
            accent_soft: rgb(0x7dcfff),
            border: rgb(0x292e42),
            border_focus: rgb(0x7aa2f7),
            text_dim: rgb(0x565f89),
            text_soft: rgb(0x9aa5ce),
            error: rgb(0xf7768e),
            success: rgb(0x9ece6a),
            warning: rgb(0xe0af68),
            user_bg: rgb(0x283457),
            tool_bg: rgb(0x2d2545),
            input_bg: rgb(0x16161e),
            selection: rgb(0x33467c),
        }
    }

    fn rose_pine() -> Self {
        Self {
            bg: rgb(0x191724),
            panel: rgb(0x1f1d2e),
            fg: rgb(0xe0def4),
            accent: rgb(0xc4a7e7),
            accent_dim: rgb(0x56526e),
            accent_soft: rgb(0xebbcba),
            border: rgb(0x26233a),
            border_focus: rgb(0xc4a7e7),
            text_dim: rgb(0x6e6a86),
            text_soft: rgb(0x908caa),
            error: rgb(0xeb6f92),
            success: rgb(0x9ccfd8),
            warning: rgb(0xf6c177),
            user_bg: rgb(0x2a273f),
            tool_bg: rgb(0x26233a),
            input_bg: rgb(0x1f1d2e),
            selection: rgb(0x403d52),
        }
    }

    fn synthwave() -> Self {
        Self {
            bg: rgb(0x2a2139),
            panel: rgb(0x241b2f),
            fg: rgb(0xf4f0fa),
            accent: rgb(0xff7edb),
            accent_dim: rgb(0x8f2d80),
            accent_soft: rgb(0x36f9f6),
            border: rgb(0x463465),
            border_focus: rgb(0xff7edb),
            text_dim: rgb(0x7a6f9b),
            text_soft: rgb(0xa99bc9),
            error: rgb(0xfe4450),
            success: rgb(0x72f1b8),
            warning: rgb(0xfede5d),
            user_bg: rgb(0x34294f),
            tool_bg: rgb(0x3b2b52),
            input_bg: rgb(0x241b2f),
            selection: rgb(0x463465),
        }
    }

    fn matrix() -> Self {
        Self {
            bg: rgb(0x050805),
            panel: rgb(0x0a120a),
            fg: rgb(0xb0ffb0),
            accent: rgb(0x00ff41),
            accent_dim: rgb(0x007828),
            accent_soft: rgb(0x80ffaa),
            border: rgb(0x143c14),
            border_focus: rgb(0x00ff41),
            text_dim: rgb(0x3c823c),
            text_soft: rgb(0x64b464),
            error: rgb(0xff5050),
            success: rgb(0x00ff41),
            warning: rgb(0xc8ff64),
            user_bg: rgb(0x0a280f),
            tool_bg: rgb(0x082014),
            input_bg: rgb(0x050f08),
            selection: rgb(0x10401c),
        }
    }

    fn paper_light() -> Self {
        Self {
            bg: rgb(0xfaf9f5),
            panel: rgb(0xf0eee7),
            fg: rgb(0x2d2b26),
            accent: rgb(0x6366f1),
            accent_dim: rgb(0xc7d2fe),
            accent_soft: rgb(0x4f46e5),
            border: rgb(0xd6d3c8),
            border_focus: rgb(0x6366f1),
            text_dim: rgb(0x928e82),
            text_soft: rgb(0x6e6b61),
            error: rgb(0xdc2626),
            success: rgb(0x16a34a),
            warning: rgb(0xca8a04),
            user_bg: rgb(0xe8e9f5),
            tool_bg: rgb(0xeee8f5),
            input_bg: rgb(0xf3f2ec),
            selection: rgb(0xdbdef3),
        }
    }

    /// The built-in preset registered under `name`, if any.
    pub fn preset(name: &str) -> Option<Theme> {
        PRESETS
            .iter()
            .find(|preset| preset.name == name)
            .map(|preset| (preset.build)())
    }

    /// The theme `name` resolves to: a `[[ui.custom_themes]]` entry first
    /// (so a custom theme may shadow a preset name it was based on), then a
    /// preset, then the default — an unknown name must never break rendering.
    pub fn resolve_name(name: &str, custom_themes: &[CustomTheme]) -> Theme {
        if let Some(custom) = custom_themes.iter().find(|theme| theme.name == name) {
            return Self::from_custom(custom);
        }
        Self::preset(name).unwrap_or_default()
    }

    /// The active theme for the `[ui]` config section.
    pub fn resolve(ui: &UiConfig) -> Theme {
        Self::resolve_name(&ui.theme, &ui.custom_themes)
    }

    /// Materialize a custom theme: its base preset (default when unknown)
    /// with every parseable `role = "#rrggbb"` override applied. Unknown
    /// keys and malformed values are skipped, not errors — a hand-edited
    /// config file degrades to the base palette instead of failing.
    pub fn from_custom(custom: &CustomTheme) -> Theme {
        let mut theme = Self::preset(&custom.base).unwrap_or_default();
        for (key, hex) in &custom.colors {
            if let Some(color) = parse_hex(hex) {
                theme.set(key, color);
            }
        }
        theme
    }

    /// The color of a role by its [`ROLES`] key.
    pub fn get(&self, key: &str) -> Option<Color> {
        Some(match key {
            "bg" => self.bg,
            "panel" => self.panel,
            "fg" => self.fg,
            "accent" => self.accent,
            "accent_dim" => self.accent_dim,
            "accent_soft" => self.accent_soft,
            "border" => self.border,
            "border_focus" => self.border_focus,
            "text_dim" => self.text_dim,
            "text_soft" => self.text_soft,
            "error" => self.error,
            "success" => self.success,
            "warning" => self.warning,
            "user_bg" => self.user_bg,
            "tool_bg" => self.tool_bg,
            "input_bg" => self.input_bg,
            "selection" => self.selection,
            _ => return None,
        })
    }

    /// Set a role by its [`ROLES`] key; returns whether the key was known.
    pub fn set(&mut self, key: &str, color: Color) -> bool {
        let slot = match key {
            "bg" => &mut self.bg,
            "panel" => &mut self.panel,
            "fg" => &mut self.fg,
            "accent" => &mut self.accent,
            "accent_dim" => &mut self.accent_dim,
            "accent_soft" => &mut self.accent_soft,
            "border" => &mut self.border,
            "border_focus" => &mut self.border_focus,
            "text_dim" => &mut self.text_dim,
            "text_soft" => &mut self.text_soft,
            "error" => &mut self.error,
            "success" => &mut self.success,
            "warning" => &mut self.warning,
            "user_bg" => &mut self.user_bg,
            "tool_bg" => &mut self.tool_bg,
            "input_bg" => &mut self.input_bg,
            "selection" => &mut self.selection,
            _ => return false,
        };
        *slot = color;
        true
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::default_dark()
    }
}

/// Parse `#rrggbb` (leading `#` optional, case-insensitive).
pub fn parse_hex(text: &str) -> Option<Color> {
    let hex = text.trim().strip_prefix('#').unwrap_or_else(|| text.trim());
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(rgb(value))
}

/// `#rrggbb` for an RGB color. Non-RGB variants never occur in themes
/// (presets and parsed customs are all `Color::Rgb`); they format as the
/// zero color rather than panicking.
pub fn to_hex(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => "#000000".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn parse_hex_accepts_both_forms_and_rejects_garbage() {
        assert_eq!(parse_hex("#ff8000"), Some(Color::Rgb(255, 128, 0)));
        assert_eq!(parse_hex("FF8000"), Some(Color::Rgb(255, 128, 0)));
        assert_eq!(parse_hex(" #ff8000 "), Some(Color::Rgb(255, 128, 0)));
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("#ff80zz"), None);
        assert_eq!(parse_hex(""), None);
    }

    #[test]
    fn hex_round_trips_through_every_role() {
        let theme = Theme::default_dark();
        for role in ROLES {
            let color = theme.get(role.key).expect("every role key resolves");
            assert_eq!(parse_hex(&to_hex(color)), Some(color), "role {}", role.key);
        }
    }

    #[test]
    fn roles_cover_every_field_and_set_works() {
        let mut theme = Theme::default_dark();
        for role in ROLES {
            assert!(theme.set(role.key, Color::Rgb(1, 2, 3)), "role {}", role.key);
            assert_eq!(theme.get(role.key), Some(Color::Rgb(1, 2, 3)));
        }
        assert!(!theme.set("nonsense", Color::Rgb(0, 0, 0)));
        assert_eq!(theme.get("nonsense"), None);
    }

    #[test]
    fn preset_names_are_unique_and_default_exists() {
        let mut names: Vec<&str> = PRESETS.iter().map(|preset| preset.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), PRESETS.len(), "duplicate preset name");
        assert!(Theme::preset("default").is_some());
        assert!(Theme::preset("no-such-theme").is_none());
    }

    #[test]
    fn resolve_prefers_custom_then_preset_then_default() {
        let custom = CustomTheme {
            name: "mine".to_string(),
            base: "dracula".to_string(),
            colors: BTreeMap::from([("accent".to_string(), "#112233".to_string())]),
        };
        let customs = vec![custom];

        let resolved = Theme::resolve_name("mine", &customs);
        assert_eq!(resolved.accent, Color::Rgb(0x11, 0x22, 0x33));
        // Untouched roles come from the base preset.
        assert_eq!(resolved.bg, Theme::preset("dracula").unwrap().bg);

        assert_eq!(Theme::resolve_name("nord", &customs), Theme::preset("nord").unwrap());
        assert_eq!(Theme::resolve_name("gone", &customs), Theme::default_dark());
    }

    #[test]
    fn from_custom_skips_unknown_keys_and_bad_hex() {
        let custom = CustomTheme {
            name: "broken".to_string(),
            base: "unknown-base".to_string(),
            colors: BTreeMap::from([
                ("accent".to_string(), "not-a-color".to_string()),
                ("mystery_role".to_string(), "#ffffff".to_string()),
            ]),
        };
        assert_eq!(Theme::from_custom(&custom), Theme::default_dark());
    }
}
