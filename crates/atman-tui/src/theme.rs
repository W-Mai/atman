use std::ops::Deref;
use std::sync::RwLock;

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeColor(Color);

impl ThemeColor {
    pub fn new(c: Color) -> Self {
        Self(c)
    }

    pub fn into_inner(self) -> Color {
        self.0
    }

    pub fn rgb(self) -> (u8, u8, u8) {
        match self.0 {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Cyan => (40, 180, 180),
            Color::DarkGray => (96, 96, 96),
            Color::Gray => (128, 128, 128),
            Color::Green => (70, 175, 70),
            Color::Yellow => (190, 175, 55),
            Color::Red => (190, 65, 65),
            Color::Blue => (0, 0, 200),
            Color::Magenta => (200, 0, 200),
            Color::LightCyan => (80, 200, 200),
            Color::LightBlue => (100, 149, 237),
            Color::LightGreen => (144, 238, 144),
            Color::LightRed => (255, 128, 128),
            Color::LightMagenta => (255, 128, 255),
            Color::LightYellow => (255, 230, 128),
            Color::White => (240, 240, 240),
            Color::Black => (16, 16, 16),
            _ => (128, 128, 128),
        }
    }

    pub fn lerp(self, other: impl Into<Color>, t: f64) -> Color {
        let (ar, ag, ab) = self.rgb();
        let other = other.into();
        let (br, bg, bb) = match other {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Cyan => (40, 180, 180),
            Color::DarkGray => (96, 96, 96),
            Color::Gray => (128, 128, 128),
            Color::Green => (70, 175, 70),
            Color::Yellow => (190, 175, 55),
            Color::Red => (190, 65, 65),
            Color::Blue => (0, 0, 200),
            Color::Magenta => (200, 0, 200),
            Color::LightCyan => (80, 200, 200),
            Color::LightBlue => (100, 149, 237),
            Color::LightGreen => (144, 238, 144),
            Color::LightRed => (255, 128, 128),
            Color::LightMagenta => (255, 128, 255),
            Color::LightYellow => (255, 230, 128),
            Color::White => (240, 240, 240),
            Color::Black => (16, 16, 16),
            _ => (128, 128, 128),
        };
        let t = t.clamp(0.0, 1.0);
        Color::Rgb(
            (ar as f64 + (br as f64 - ar as f64) * t).round() as u8,
            (ag as f64 + (bg as f64 - ag as f64) * t).round() as u8,
            (ab as f64 + (bb as f64 - ab as f64) * t).round() as u8,
        )
    }
}

impl From<Color> for ThemeColor {
    fn from(c: Color) -> Self {
        Self(c)
    }
}

impl From<ThemeColor> for Color {
    fn from(c: ThemeColor) -> Self {
        c.0
    }
}

impl Deref for ThemeColor {
    type Target = Color;
    fn deref(&self) -> &Color {
        &self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub mode: ThemeMode,
    pub code_bg: ThemeColor,
    pub user_msg_bg: ThemeColor,
    pub note_info_bg: ThemeColor,
    pub note_warn_bg: ThemeColor,
    pub note_error_bg: ThemeColor,
    pub note_success_bg: ThemeColor,
    pub note_debug_bg: ThemeColor,
    pub tinted_fg: ThemeColor,
    pub subtle_fg: ThemeColor,
    pub modal_bg: ThemeColor,
    pub panel_bg: ThemeColor,
    pub accent: ThemeColor,
    pub success: ThemeColor,
    pub warn: ThemeColor,
    pub error: ThemeColor,
    pub highlight_bg: ThemeColor,
    pub border: ThemeColor,
    pub heading: ThemeColor,
    pub meta_fg: ThemeColor,
}

impl Theme {
    fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            code_bg: ThemeColor(Color::Rgb(22, 24, 28)),
            user_msg_bg: ThemeColor(Color::Rgb(38, 42, 54)),
            note_info_bg: ThemeColor(Color::Rgb(20, 26, 34)),
            note_warn_bg: ThemeColor(Color::Rgb(38, 30, 16)),
            note_error_bg: ThemeColor(Color::Rgb(40, 20, 22)),
            note_success_bg: ThemeColor(Color::Rgb(18, 32, 22)),
            note_debug_bg: ThemeColor(Color::Rgb(22, 22, 24)),
            tinted_fg: ThemeColor(Color::Gray),
            subtle_fg: ThemeColor(Color::Rgb(96, 96, 96)),
            modal_bg: ThemeColor(Color::Rgb(12, 14, 18)),
            panel_bg: ThemeColor(Color::Rgb(28, 30, 36)),
            accent: ThemeColor(Color::Cyan),
            success: ThemeColor(Color::Green),
            warn: ThemeColor(Color::Yellow),
            error: ThemeColor(Color::Red),
            highlight_bg: ThemeColor(Color::DarkGray),
            border: ThemeColor(Color::DarkGray),
            heading: ThemeColor(Color::Cyan),
            meta_fg: ThemeColor(Color::DarkGray),
        }
    }

    fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            code_bg: ThemeColor(Color::Rgb(240, 240, 240)),
            user_msg_bg: ThemeColor(Color::Rgb(220, 225, 235)),
            note_info_bg: ThemeColor(Color::Rgb(220, 232, 244)),
            note_warn_bg: ThemeColor(Color::Rgb(248, 236, 210)),
            note_error_bg: ThemeColor(Color::Rgb(250, 220, 220)),
            note_success_bg: ThemeColor(Color::Rgb(210, 240, 210)),
            note_debug_bg: ThemeColor(Color::Rgb(235, 235, 240)),
            tinted_fg: ThemeColor(Color::Rgb(30, 30, 30)),
            subtle_fg: ThemeColor(Color::Rgb(96, 96, 96)),
            modal_bg: ThemeColor(Color::Rgb(250, 250, 250)),
            panel_bg: ThemeColor(Color::Rgb(232, 232, 236)),
            accent: ThemeColor(Color::Rgb(0, 120, 160)),
            success: ThemeColor(Color::Rgb(0, 128, 0)),
            warn: ThemeColor(Color::Rgb(180, 130, 0)),
            error: ThemeColor(Color::Rgb(180, 0, 0)),
            highlight_bg: ThemeColor(Color::Rgb(200, 220, 240)),
            border: ThemeColor(Color::Rgb(180, 180, 180)),
            heading: ThemeColor(Color::Rgb(0, 100, 140)),
            meta_fg: ThemeColor(Color::Rgb(140, 140, 140)),
        }
    }
}

static THEME: RwLock<Option<Theme>> = RwLock::new(None);

pub fn theme() -> Theme {
    if let Some(t) = *THEME.read().unwrap() {
        return t;
    }
    let t = build_theme(detect_mode());
    *THEME.write().unwrap() = Some(t);
    t
}

pub fn current_mode() -> ThemeMode {
    theme().mode
}

pub fn set_mode(mode: ThemeMode) -> bool {
    let mut slot = THEME.write().unwrap();
    let changed = slot.map(|t| t.mode) != Some(mode);
    *slot = Some(build_theme(mode));
    changed
}

fn build_theme(mode: ThemeMode) -> Theme {
    match mode {
        ThemeMode::Light => Theme::light(),
        ThemeMode::Dark => Theme::dark(),
    }
}

pub fn detect_mode() -> ThemeMode {
    if let Ok(v) = std::env::var("ATMAN_THEME") {
        match v.to_ascii_lowercase().as_str() {
            "light" => return ThemeMode::Light,
            "dark" => return ThemeMode::Dark,
            _ => {}
        }
    }
    if let Some(mode) = read_config_theme_mode() {
        return mode;
    }
    let mut opts = terminal_colorsaurus::QueryOptions::default();
    opts.timeout = std::time::Duration::from_millis(80);
    match terminal_colorsaurus::theme_mode(opts) {
        Ok(terminal_colorsaurus::ThemeMode::Light) => ThemeMode::Light,
        Ok(terminal_colorsaurus::ThemeMode::Dark) => ThemeMode::Dark,
        _ => ThemeMode::Dark,
    }
}

fn read_config_theme_mode() -> Option<ThemeMode> {
    let cfg = atman_runtime::storage::config_dir().ok()?;
    let text = std::fs::read_to_string(cfg.join("config.toml")).ok()?;
    let mut in_section = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[')
            && let Some(name) = rest.strip_suffix(']')
        {
            in_section = name.trim() == "theme";
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if k == "mode" {
                return match v.to_ascii_lowercase().as_str() {
                    "light" => Some(ThemeMode::Light),
                    "dark" => Some(ThemeMode::Dark),
                    _ => None,
                };
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lerp_at_zero_returns_a() {
        let a = ThemeColor(Color::Rgb(0, 0, 0));
        let b = ThemeColor(Color::Rgb(100, 100, 100));
        assert_eq!(a.lerp(b, 0.0), Color::Rgb(0, 0, 0));
    }

    #[test]
    fn lerp_at_one_returns_b() {
        let a = ThemeColor(Color::Rgb(0, 0, 0));
        let b = Color::Rgb(100, 100, 100);
        assert_eq!(a.lerp(b, 1.0), Color::Rgb(100, 100, 100));
    }

    #[test]
    fn lerp_at_half_is_midpoint() {
        let a = ThemeColor(Color::Rgb(0, 0, 0));
        let b = Color::Rgb(100, 100, 100);
        assert_eq!(a.lerp(b, 0.5), Color::Rgb(50, 50, 50));
    }

    #[test]
    fn lerp_clamps_t() {
        let a = ThemeColor(Color::Rgb(0, 0, 0));
        let b = Color::Rgb(100, 100, 100);
        assert_eq!(a.lerp(b, -1.0), Color::Rgb(0, 0, 0));
        assert_eq!(a.lerp(b, 2.0), Color::Rgb(100, 100, 100));
    }

    #[test]
    fn lerp_accepts_raw_color() {
        let a = ThemeColor(Color::Rgb(0, 0, 0));
        assert_eq!(a.lerp(Color::Rgb(100, 0, 0), 1.0), Color::Rgb(100, 0, 0));
    }

    #[test]
    fn deref_exposes_inner_color() {
        let c = ThemeColor(Color::Cyan);
        assert_eq!(*c, Color::Cyan);
    }

    #[test]
    fn into_color_roundtrip() {
        let original = Color::Rgb(42, 42, 42);
        let themed: ThemeColor = original.into();
        let back: Color = themed.into();
        assert_eq!(back, original);
    }

    #[test]
    fn rgb_handles_named_colors() {
        assert_eq!(ThemeColor(Color::Cyan).rgb(), (40, 180, 180));
        assert_eq!(ThemeColor(Color::White).rgb(), (240, 240, 240));
        assert_eq!(ThemeColor(Color::Black).rgb(), (16, 16, 16));
    }
}
