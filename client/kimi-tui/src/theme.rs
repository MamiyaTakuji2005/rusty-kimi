//! Terminal palette, mirroring the archived Python shell's `ui/theme.py`
//! aesthetic: no boxes, hairline `─` separators, one blue/cyan accent family,
//! dim greys for chrome. Colors are ANSI so they follow the terminal scheme;
//! the names describe *roles*, not exact hues.

use ratatui::style::{Color, Modifier, Style};

/// Accent for the app title and primary highlights (Python shell: cyan/blue).
pub const ACCENT: Color = Color::Blue;

/// Brighter companion of [`ACCENT`] for symbols that must pop (✨, spinner).
pub const ACCENT_BRIGHT: Color = Color::Cyan;

/// Chrome color: separators, meta text, tool-output tails (shell: `#4a5568`).
pub const DIM: Color = Color::DarkGray;

/// Warnings and pending states.
pub const WARNING: Color = Color::Yellow;

/// Errors and rejections.
pub const ERROR: Color = Color::Red;

/// Success marks (✓, approved).
pub const SUCCESS: Color = Color::Green;

pub fn title() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT_BRIGHT)
}

pub fn dim() -> Style {
    Style::default().fg(DIM)
}

#[allow(dead_code)] // part of the palette; used as soon as a warning role exists
pub fn warning() -> Style {
    Style::default().fg(WARNING)
}

pub fn error() -> Style {
    Style::default().fg(ERROR)
}

pub fn success() -> Style {
    Style::default().fg(SUCCESS)
}

/// The hairline between transcript and input: `── label ────────────`,
/// echoing the Python shell's input-panel header without any box.
pub fn separator_line(label: &str, width: u16) -> String {
    let mut out = String::from("── ");
    out.push_str(label);
    let used = 3 + label.chars().count();
    let fill = (width as usize).saturating_sub(used);
    out.push_str(&"─".repeat(fill));
    out
}
