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
    pub modal_bg: Color,
    pub panel_bg: Color,
    pub accent: Color,
    pub success: Color,
    pub warn: Color,
    pub error: Color,
    pub highlight_bg: Color,
    pub border: Color,
    pub heading: Color,
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
            modal_bg: Color::Rgb(12, 14, 18),
            panel_bg: Color::Rgb(28, 30, 36),
            accent: Color::Cyan,
            success: Color::Green,
            warn: Color::Yellow,
            error: Color::Red,
            highlight_bg: Color::DarkGray,
            border: Color::DarkGray,
            heading: Color::Cyan,
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
            modal_bg: Color::Rgb(250, 250, 250),
            panel_bg: Color::Rgb(232, 232, 236),
            accent: Color::Rgb(0, 120, 160),
            success: Color::Rgb(0, 128, 0),
            warn: Color::Rgb(180, 130, 0),
            error: Color::Rgb(180, 0, 0),
            highlight_bg: Color::Rgb(200, 220, 240),
            border: Color::Rgb(180, 180, 180),
            heading: Color::Rgb(0, 100, 140),
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
