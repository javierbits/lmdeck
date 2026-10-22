//! Colour themes. The default borrows oh-my-pi's dark palette (amber accent,
//! blue borders, powerline status segments); the others are Catppuccin Mocha
//! and Tokyo Night.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub name: &'static str,
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
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const OMP: Theme = Theme {
    name: "omp",
    bg: rgb(0x18181e),
    panel: rgb(0x1e1e24),
    fg: rgb(0xd4d4d4),
    muted: rgb(0x777d88),
    dim: rgb(0x5f6673),
    accent: rgb(0xfebc38),
    border: rgb(0x178fb9),
    border_focus: rgb(0x0088fa),
    selected_bg: rgb(0x31363f),
    ok: rgb(0x89d281),
    warn: rgb(0xe4c00f),
    err: rgb(0xfc3a4b),
    info: rgb(0x00afaf),
    on_pill: rgb(0x121212),
    bar_bg: rgb(0x121212),
    seg: [rgb(0xd787af), rgb(0x00afaf), rgb(0x8787af), rgb(0x5faf5f)],
    gauge_bg: rgb(0x3d424a),
};

pub const CATPPUCCIN: Theme = Theme {
    name: "catppuccin",
    bg: rgb(0x1e1e2e),
    panel: rgb(0x181825),
    fg: rgb(0xcdd6f4),
    muted: rgb(0xa6adc8),
    dim: rgb(0x6c7086),
    accent: rgb(0xfab387),
    border: rgb(0x585b70),
    border_focus: rgb(0x89b4fa),
    selected_bg: rgb(0x313244),
    ok: rgb(0xa6e3a1),
    warn: rgb(0xf9e2af),
    err: rgb(0xf38ba8),
    info: rgb(0x89dceb),
    on_pill: rgb(0x11111b),
    bar_bg: rgb(0x11111b),
    seg: [rgb(0xcba6f7), rgb(0x94e2d5), rgb(0xb4befe), rgb(0xa6e3a1)],
    gauge_bg: rgb(0x45475a),
};

pub const TOKYO_NIGHT: Theme = Theme {
    name: "tokyo-night",
    bg: rgb(0x1a1b26),
    panel: rgb(0x16161e),
    fg: rgb(0xc0caf5),
    muted: rgb(0xa9b1d6),
    dim: rgb(0x565f89),
    accent: rgb(0xff9e64),
    border: rgb(0x3b4261),
    border_focus: rgb(0x7aa2f7),
    selected_bg: rgb(0x292e42),
    ok: rgb(0x9ece6a),
    warn: rgb(0xe0af68),
    err: rgb(0xf7768e),
    info: rgb(0x7dcfff),
    on_pill: rgb(0x15161e),
    bar_bg: rgb(0x15161e),
    seg: [rgb(0xbb9af7), rgb(0x7dcfff), rgb(0x7aa2f7), rgb(0x9ece6a)],
    gauge_bg: rgb(0x414868),
};

pub const THEMES: [Theme; 3] = [OMP, CATPPUCCIN, TOKYO_NIGHT];

/// Looks a theme up by name (`omp`/`dark`, `catppuccin`/`mocha`, `tokyo-night`).
pub fn by_name(name: &str) -> Option<usize> {
    let n = name.to_ascii_lowercase().replace(['_', ' '], "-");
    match n.as_str() {
        "omp" | "dark" | "default" | "oh-my-pi" => Some(0),
        "catppuccin" | "mocha" | "catppuccin-mocha" => Some(1),
        "tokyo-night" | "tokyonight" | "tokyo" => Some(2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resolve() {
        assert_eq!(by_name("Dark"), Some(0));
        assert_eq!(by_name("mocha"), Some(1));
        assert_eq!(by_name("tokyo_night"), Some(2));
        assert_eq!(by_name("solarized"), None);
        for (i, t) in THEMES.iter().enumerate() {
            assert_eq!(by_name(t.name), Some(i));
        }
    }
}
