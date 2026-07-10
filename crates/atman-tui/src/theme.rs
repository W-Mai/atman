use std::sync::RwLock;

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub mode: ThemeMode,
    pub code_bg: Color,
    pub user_msg_bg: Color,
    pub note_info_bg: Color,
    pub note_warn_bg: Color,
    pub note_error_bg: Color,
    pub tinted_fg: Color,
    pub subtle_fg: Color,
}

impl Theme {
    fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            code_bg: Color::Rgb(22, 24, 28),
            user_msg_bg: Color::Rgb(38, 42, 54),
            note_info_bg: Color::Rgb(20, 26, 34),
            note_warn_bg: Color::Rgb(38, 30, 16),
            note_error_bg: Color::Rgb(40, 20, 22),
            tinted_fg: Color::Gray,
            subtle_fg: Color::DarkGray,
        }
    }

    fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            code_bg: Color::Rgb(240, 240, 240),
            user_msg_bg: Color::Rgb(220, 225, 235),
            note_info_bg: Color::Rgb(220, 232, 244),
            note_warn_bg: Color::Rgb(248, 236, 210),
            note_error_bg: Color::Rgb(250, 220, 220),
            tinted_fg: Color::Rgb(30, 30, 30),
            subtle_fg: Color::Rgb(96, 96, 96),
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
    let mut opts = terminal_colorsaurus::QueryOptions::default();
    opts.timeout = std::time::Duration::from_millis(80);
    match terminal_colorsaurus::theme_mode(opts) {
        Ok(terminal_colorsaurus::ThemeMode::Light) => ThemeMode::Light,
        Ok(terminal_colorsaurus::ThemeMode::Dark) => ThemeMode::Dark,
        _ => ThemeMode::Dark,
    }
}
