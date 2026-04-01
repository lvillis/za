use crossterm::style::available_color_count;
use std::{
    env,
    io::{self, IsTerminal},
    sync::atomic::{AtomicU8, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Success,
    Warning,
    Error,
    Active,
    Header,
    Dim,
}

static COLOR_MODE: AtomicU8 = AtomicU8::new(ColorMode::Auto as u8);

pub fn set_color_mode(mode: ColorMode) {
    COLOR_MODE.store(mode as u8, Ordering::Relaxed);
}

pub fn stdout_color_enabled() -> bool {
    color_enabled(io::stdout().is_terminal())
}

pub fn paint(value: impl AsRef<str>, tone: Tone) -> String {
    let value = value.as_ref();
    if !stdout_color_enabled() || value.is_empty() {
        return value.to_string();
    }

    let codes = match tone {
        Tone::Success => ["32", "1"].as_slice(),
        Tone::Warning => ["33", "1"].as_slice(),
        Tone::Error => ["31", "1"].as_slice(),
        Tone::Active => ["36", "1"].as_slice(),
        Tone::Header => ["1"].as_slice(),
        Tone::Dim => ["2"].as_slice(),
    };
    style_ansi(value, codes)
}

pub fn success(value: impl AsRef<str>) -> String {
    paint(value, Tone::Success)
}

pub fn warning(value: impl AsRef<str>) -> String {
    paint(value, Tone::Warning)
}

pub fn error(value: impl AsRef<str>) -> String {
    paint(value, Tone::Error)
}

pub fn active(value: impl AsRef<str>) -> String {
    paint(value, Tone::Active)
}

pub fn header(value: impl AsRef<str>) -> String {
    paint(value, Tone::Header)
}

pub fn dim(value: impl AsRef<str>) -> String {
    paint(value, Tone::Dim)
}

fn color_enabled(is_terminal: bool) -> bool {
    match current_color_mode() {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => auto_color_enabled(is_terminal),
    }
}

fn current_color_mode() -> ColorMode {
    match COLOR_MODE.load(Ordering::Relaxed) {
        value if value == ColorMode::Always as u8 => ColorMode::Always,
        value if value == ColorMode::Never as u8 => ColorMode::Never,
        _ => ColorMode::Auto,
    }
}

fn auto_color_enabled(is_terminal: bool) -> bool {
    if env::var_os("NO_COLOR").is_some()
        || matches!(env::var("CLICOLOR").ok().as_deref(), Some("0"))
        || matches!(env::var("TERM").ok().as_deref(), Some("dumb"))
    {
        return false;
    }

    if matches!(env::var("CLICOLOR_FORCE").ok().as_deref(), Some(value) if value != "0") {
        return true;
    }

    is_terminal && available_color_count() >= 8
}

fn style_ansi(value: &str, codes: &[&str]) -> String {
    format!("\u{1b}[{}m{value}\u{1b}[0m", codes.join(";"))
}
