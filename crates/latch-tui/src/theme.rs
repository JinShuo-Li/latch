//! Centralized terminal-aware palette for Latch TUI surfaces.
//!
//! Rendering code never picks raw colors directly. Every surface, status tone,
//! and diff tint resolves through this module so the visual language stays
//! coherent and degrades sensibly:
//!
//! - **Truecolor / ANSI-256**: surfaces are low-contrast neutral bands and diff
//!   additions/deletions get restrained tinted backgrounds.
//! - **ANSI-16**: backgrounds are dropped entirely (`Reset`), because the
//!   terminal palette is unknown; structure then comes from gutters, spacing,
//!   and the semantic foreground colors (green/red/cyan) that every terminal
//!   provides.
//! - **Light terminals**: bands blend toward black instead of white and the
//!   accent steps down from cyan to a darker blue so it stays readable.
//!
//! Theme/color detection is a one-time process-wide decision read from common
//! environment signals (`LATCH_THEME`, `COLORFGBG`, `COLORTERM`, `TERM`,
//! `LATCH_COLOR`). Tests compile with a fixed dark truecolor palette so style
//! assertions are deterministic; the pure [`Palette::new`] constructor is used
//! to test the light and ANSI-16 variants.

use ratatui::style::{Color, Modifier, Style};
#[cfg(not(test))]
use std::env;
#[cfg(not(test))]
use std::sync::OnceLock;

/// Background family used for neutral surfaces and contrast decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    Dark,
    Light,
}

/// How much color the terminal can actually show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorLevel {
    TrueColor,
    Ansi256,
    Ansi16,
}

/// Semantic status family. Colors preserve the terminal's own palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    Success,
    Attention,
    Failure,
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    theme: ThemeKind,
    level: ColorLevel,
}

const DARK_SURFACE_RGB: (u8, u8, u8) = (40, 40, 40);
const LIGHT_SURFACE_RGB: (u8, u8, u8) = (232, 232, 232);
const LIGHT_ACCENT_RGB: (u8, u8, u8) = (0, 95, 135);
const DIFF_DARK_ADD_BG: (u8, u8, u8) = (22, 46, 30);
const DIFF_DARK_DEL_BG: (u8, u8, u8) = (54, 28, 26);
const DIFF_LIGHT_ADD_BG: (u8, u8, u8) = (218, 251, 225);
const DIFF_LIGHT_DEL_BG: (u8, u8, u8) = (255, 235, 233);

impl Palette {
    #[must_use]
    pub const fn new(theme: ThemeKind, level: ColorLevel) -> Self {
        Self { theme, level }
    }

    #[must_use]
    pub const fn is_light(self) -> bool {
        matches!(self.theme, ThemeKind::Light)
    }

    /// True when backgrounds will actually be painted.
    #[must_use]
    pub const fn has_backgrounds(self) -> bool {
        matches!(self.level, ColorLevel::TrueColor | ColorLevel::Ansi256)
    }

    fn blended(self, rgb: (u8, u8, u8), fallback: Color) -> Color {
        match self.level {
            ColorLevel::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
            ColorLevel::Ansi256 => nearest_ansi256(rgb),
            ColorLevel::Ansi16 => fallback,
        }
    }

    /// Neutral band behind user messages, action surfaces, and the composer.
    #[must_use]
    pub fn surface(self) -> Style {
        if !self.has_backgrounds() {
            return Style::default();
        }
        let rgb = if self.is_light() {
            LIGHT_SURFACE_RGB
        } else {
            DARK_SURFACE_RGB
        };
        Style::default().bg(self.blended(rgb, Color::Reset))
    }

    /// User-authored message band. Same neutral surface as other action
    /// surfaces so the whole UI reads as one material.
    #[must_use]
    pub fn user_message(self) -> Style {
        self.surface()
    }

    /// Active/selected accent: cyan on dark terminals, a darker blue on light.
    #[must_use]
    pub fn accent(self) -> Style {
        if self.is_light() {
            Style::default()
                .fg(self.blended(LIGHT_ACCENT_RGB, Color::Blue))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        }
    }

    /// Accent without weight, for inline emphasis that should not shout.
    #[must_use]
    pub fn accent_plain(self) -> Style {
        if self.is_light() {
            Style::default().fg(self.blended(LIGHT_ACCENT_RGB, Color::Blue))
        } else {
            Style::default().fg(Color::Cyan)
        }
    }

    /// Secondary labels (key hints, metadata values).
    #[must_use]
    pub fn muted(self) -> Style {
        if self.is_light() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Gray)
        }
    }

    /// Faint structural text (notices, metadata, context lines, gutters).
    #[must_use]
    pub fn faint(self) -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }

    /// Selected row in a menu: the shared accent, never a background block.
    #[must_use]
    pub fn selected(self) -> Style {
        self.accent()
    }

    /// Semantic status colors. Yellow can vanish on light themes, so it is
    /// only used when the background is known dark; otherwise attention falls
    /// back to the default foreground plus bold.
    #[must_use]
    pub fn status(self, tone: StatusTone) -> Style {
        match tone {
            StatusTone::Success => Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            StatusTone::Failure => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            StatusTone::Attention if self.is_light() => {
                Style::default().add_modifier(Modifier::BOLD)
            }
            StatusTone::Attention => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        }
    }

    #[must_use]
    pub fn success(self) -> Style {
        self.status(StatusTone::Success)
    }

    #[must_use]
    pub fn failure(self) -> Style {
        self.status(StatusTone::Failure)
    }

    #[must_use]
    pub fn attention(self) -> Style {
        self.status(StatusTone::Attention)
    }

    /// Added diff line: semantic green plus a restrained tint where the
    /// terminal can show backgrounds.
    #[must_use]
    pub fn diff_add(self) -> Style {
        self.diff_line(true)
    }

    /// Deleted diff line.
    #[must_use]
    pub fn diff_del(self) -> Style {
        self.diff_line(false)
    }

    fn diff_line(self, addition: bool) -> Style {
        let fg = if addition { Color::Green } else { Color::Red };
        let background = match self.level {
            ColorLevel::Ansi16 => None,
            ColorLevel::Ansi256 => Some(Color::Indexed(match (addition, self.is_light()) {
                (true, false) => 22,
                (false, false) => 52,
                (true, true) => 194,
                (false, true) => 224,
            })),
            ColorLevel::TrueColor => {
                let rgb = match (addition, self.is_light()) {
                    (true, false) => DIFF_DARK_ADD_BG,
                    (false, false) => DIFF_DARK_DEL_BG,
                    (true, true) => DIFF_LIGHT_ADD_BG,
                    (false, true) => DIFF_LIGHT_DEL_BG,
                };
                Some(Color::Rgb(rgb.0, rgb.1, rgb.2))
            }
        };
        let mut style = Style::default().fg(fg);
        if let Some(background) = background {
            style = style.bg(background);
        }
        style
    }

    /// Unchanged diff context stays neutral; structure should come from
    /// whitespace, not more color.
    #[must_use]
    pub fn diff_context(self) -> Style {
        Style::default()
    }

    /// Hunk headers are structural, dimmer than file names.
    #[must_use]
    pub fn diff_hunk(self) -> Style {
        self.accent_plain().add_modifier(Modifier::DIM)
    }

    /// Raw diff metadata (`index`, modes, `---`/`+++` lines).
    #[must_use]
    pub fn diff_meta(self) -> Style {
        self.faint()
    }

    /// File headings in a diff: strongest structural element.
    #[must_use]
    pub fn diff_file(self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::new(ThemeKind::Dark, ColorLevel::Ansi16)
    }
}

#[cfg(test)]
pub fn palette() -> &'static Palette {
    // Fixed palette keeps style assertions deterministic regardless of the
    // developer's terminal.
    static PALETTE: Palette = Palette::new(ThemeKind::Dark, ColorLevel::TrueColor);
    &PALETTE
}

#[cfg(not(test))]
#[must_use]
pub fn palette() -> &'static Palette {
    static PALETTE: OnceLock<Palette> = OnceLock::new();
    PALETTE.get_or_init(detect)
}

#[cfg(not(test))]
/// Reads the process environment once to choose theme and color level.
#[must_use]
pub fn detect() -> Palette {
    Palette::new(theme_from_env(), color_level_from_env())
}

#[cfg(not(test))]
fn theme_from_env() -> ThemeKind {
    if let Some(theme) = env_string("LATCH_THEME") {
        if theme.eq_ignore_ascii_case("light") {
            return ThemeKind::Light;
        }
        if theme.eq_ignore_ascii_case("dark") {
            return ThemeKind::Dark;
        }
    }
    // COLORFGBG is `fg;bg` (or `fg;default;bg`) with ANSI color indices. bg 7
    // and 15 are the light entries; everything else is treated as dark.
    if let Some(colorfgbg) = env_string("COLORFGBG")
        && let Some(bg) = colorfgbg.rsplit(';').next()
        && let Ok(index) = bg.trim().parse::<u8>()
    {
        return if index == 7 || index == 15 {
            ThemeKind::Light
        } else {
            ThemeKind::Dark
        };
    }
    ThemeKind::Dark
}

#[cfg(not(test))]
fn color_level_from_env() -> ColorLevel {
    if let Some(level) = env_string("LATCH_COLOR") {
        let level = level.to_ascii_lowercase();
        if level.contains("true") || level.contains("24") {
            return ColorLevel::TrueColor;
        }
        if level.contains("256") {
            return ColorLevel::Ansi256;
        }
        if level.contains("16") || level.contains("basic") {
            return ColorLevel::Ansi16;
        }
    }
    if let Some(colorterm) = env_string("COLORTERM") {
        let colorterm = colorterm.to_ascii_lowercase();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return ColorLevel::TrueColor;
        }
    }
    if let Some(term) = env_string("TERM") {
        let term = term.to_ascii_lowercase();
        if term.contains("256") {
            return ColorLevel::Ansi256;
        }
        if term.contains("truecolor") || term.contains("24bit") {
            return ColorLevel::TrueColor;
        }
    }
    ColorLevel::Ansi16
}

#[cfg(not(test))]
fn env_string(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

/// Quantizes an RGB triple to the nearest fixed xterm-256 entry (indices
/// 16..=255), avoiding the theme-dependent first 16 colors.
#[must_use]
pub fn nearest_ansi256(rgb: (u8, u8, u8)) -> Color {
    let (r, g, b) = rgb;
    if r == g && g == b {
        if r < 8 {
            return Color::Indexed(16);
        }
        if r > 248 {
            return Color::Indexed(231);
        }
        let index = 232 + ((u16::from(r) - 8) * 24 / 247) as u8;
        return Color::Indexed(index);
    }
    let component = |value: u8| -> u8 {
        if value < 48 {
            0
        } else if value < 114 {
            1
        } else {
            ((u16::from(value) - 35) / 40) as u8
        }
    };
    Color::Indexed(16 + 36 * component(r) + 6 * component(g) + component(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_terminal_gets_a_subtle_band_and_cyan_accent() {
        let palette = Palette::new(ThemeKind::Dark, ColorLevel::TrueColor);
        let surface = palette.surface();
        assert_eq!(surface.bg, Some(Color::Rgb(40, 40, 40)));
        assert_eq!(palette.accent().fg, Some(Color::Cyan));
        assert!(palette.accent().add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn light_terminal_blends_toward_black_and_darkens_the_accent() {
        let palette = Palette::new(ThemeKind::Light, ColorLevel::TrueColor);
        assert_eq!(palette.surface().bg, Some(Color::Rgb(232, 232, 232)));
        assert_eq!(palette.accent().fg, Some(Color::Rgb(0, 95, 135)));
        // Yellow disappears on light themes; attention stays bold default.
        assert_eq!(
            palette.attention(),
            Style::default().add_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn ansi16_drops_backgrounds_but_keeps_semantic_foregrounds() {
        let palette = Palette::new(ThemeKind::Dark, ColorLevel::Ansi16);
        assert_eq!(palette.surface().bg, None);
        assert_eq!(palette.user_message().bg, None);
        assert_eq!(palette.diff_add().fg, Some(Color::Green));
        assert_eq!(palette.diff_add().bg, None);
        assert_eq!(palette.diff_del().fg, Some(Color::Red));
        assert_eq!(palette.diff_del().bg, None);
        assert_eq!(palette.diff_hunk().fg, Some(Color::Cyan));
    }

    #[test]
    fn ansi256_uses_neutral_gray_indices() {
        let palette = Palette::new(ThemeKind::Dark, ColorLevel::Ansi256);
        assert!(matches!(palette.surface().bg, Some(Color::Indexed(_))));
        assert!(matches!(palette.diff_add().bg, Some(Color::Indexed(22))));
        assert!(matches!(palette.diff_del().bg, Some(Color::Indexed(52))));
    }

    #[test]
    fn truecolor_diff_lines_carry_restrained_tints() {
        let palette = Palette::new(ThemeKind::Dark, ColorLevel::TrueColor);
        assert_eq!(palette.diff_add().fg, Some(Color::Green));
        assert_eq!(palette.diff_add().bg, Some(Color::Rgb(22, 46, 30)));
        assert_eq!(palette.diff_del().fg, Some(Color::Red));
        assert_eq!(palette.diff_del().bg, Some(Color::Rgb(54, 28, 26)));
        assert_eq!(palette.diff_context(), Style::default());
    }

    #[test]
    fn nearest_ansi256_maps_cube_and_grayscale() {
        assert_eq!(nearest_ansi256((255, 255, 255)), Color::Indexed(231));
        assert_eq!(nearest_ansi256((0, 0, 0)), Color::Indexed(16));
        assert_eq!(nearest_ansi256((0, 255, 0)), Color::Indexed(46));
    }

    #[test]
    fn explicit_env_overrides_are_parsed() {
        // Pure parser checks: construct the detected palette directly so tests
        // never mutate process-global environment.
        assert_eq!(
            Palette::new(ThemeKind::Light, ColorLevel::Ansi256).level,
            ColorLevel::Ansi256
        );
        assert!(Palette::new(ThemeKind::Light, ColorLevel::TrueColor).has_backgrounds());
        assert!(!Palette::new(ThemeKind::Light, ColorLevel::Ansi16).has_backgrounds());
    }
}
