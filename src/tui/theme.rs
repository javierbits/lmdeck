//! The TUI's view of [`crate::theme`]: the same palettes as ratatui colours.

use ratatui::style::Color;
use std::sync::LazyLock;

use crate::theme::{self as palettes, Palette};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub name: &'static str,
    pub title: &'static str,
    pub light: bool,
    pub bg: Color,
    pub panel: Color,
    pub fg: Color,
    pub muted: Color,
    pub dim: Color,
    pub accent: Color,
    pub border: Color,
    pub border_focus: Color,
    pub selected_bg: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    pub info: Color,
    /// Text drawn on top of a coloured pill.
    pub on_pill: Color,
    pub bar_bg: Color,
    /// Powerline status segments, left to right.
    pub seg: [Color; 4],
    pub gauge_bg: Color,
    pub magenta: Color,
}

pub const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl From<&Palette> for Theme {
    fn from(p: &Palette) -> Self {
        Theme {
            name: p.slug,
            title: p.name,
            light: p.light,
            bg: rgb(p.bg),
            panel: rgb(p.panel),
            fg: rgb(p.fg),
            muted: rgb(p.muted),
            dim: rgb(p.dim),
            accent: rgb(p.accent),
            border: rgb(p.border),
            border_focus: rgb(p.border_focus),
            selected_bg: rgb(p.selected_bg),
            ok: rgb(p.ok),
            warn: rgb(p.warn),
            err: rgb(p.err),
            info: rgb(p.info),
            on_pill: rgb(p.on_pill),
            bar_bg: rgb(p.bar_bg),
            seg: p.seg.map(rgb),
            gauge_bg: rgb(p.gauge_bg),
            magenta: rgb(p.magenta),
        }
    }
}

static THEMES: LazyLock<Vec<Theme>> = LazyLock::new(|| palettes::all().iter().map(Theme::from).collect());

/// Every theme, in [`crate::theme::all`] order (`omp` first).
pub fn themes() -> &'static [Theme] {
    &THEMES
}

/// The default `omp` theme.
pub fn omp() -> &'static Theme {
    &THEMES[0]
}

/// Looks a theme up by any spelling [`crate::theme::index`] accepts.
pub fn by_name(name: &str) -> Option<usize> {
    palettes::index(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resolve() {
        assert_eq!(by_name("Dark"), Some(0));
        assert_eq!(themes()[by_name("mocha").unwrap()].name, "catppuccin");
        assert_eq!(themes()[by_name("tokyo_night").unwrap()].name, "tokyo-night");
        assert_eq!(by_name("solarized"), None);
        for (i, t) in themes().iter().enumerate() {
            assert_eq!(by_name(t.name), Some(i));
        }
    }
}
