//! Colour themes, shared by the TUI, the web dashboard and `lmdeck theme`.
//!
//! `retro-82` (Retro 82) is the default. `omp` is lmdeck's own hand-tuned
//! palette; every other theme is one of Omarchy's (<https://omarchy.org>, MIT):
//! `omarchy.rs` is generated from
//! `themes/<slug>/colors.toml` by `scripts/import-omarchy-themes.py`, colours
//! verbatim. [`Palette::from_omarchy`] maps those colours onto lmdeck's roles.
//! For each role it takes the first palette colour that is readable against
//! what it's drawn on (for example `yellow`, else `bright_yellow`, else
//! `orange`), so light themes stay legible; it never mixes new colours.

mod omarchy;

use serde::Serialize;
use std::path::PathBuf;
use std::sync::LazyLock;

pub use omarchy::{OMARCHY, OMARCHY_COMMIT};

/// One Omarchy `colors.toml`, as shipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Omarchy {
    pub slug: &'static str,
    pub name: &'static str,
    pub light: bool,
    pub accent: u32,
    pub selection: u32,
    pub muted: u32,
    pub background: u32,
    pub dark_background: u32,
    pub darker_background: u32,
    pub lighter_background: u32,
    pub foreground: u32,
    pub dark_foreground: u32,
    pub light_foreground: u32,
    pub bright_foreground: u32,
    pub red: u32,
    pub yellow: u32,
    pub green: u32,
    pub cyan: u32,
    pub blue: u32,
    pub magenta: u32,
    pub bright_red: u32,
    pub bright_yellow: u32,
    pub bright_green: u32,
    pub bright_cyan: u32,
    pub bright_blue: u32,
    pub bright_magenta: u32,
    pub orange: Option<u32>,
    pub brown: Option<u32>,
}

/// lmdeck's colour roles, as `0xRRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Palette {
    pub slug: &'static str,
    pub name: &'static str,
    pub light: bool,
    pub bg: u32,
    /// Panels and cards, a step off the background.
    pub panel: u32,
    /// A lighter panel surface (the top of a card's gradient).
    pub panel2: u32,
    pub fg: u32,
    /// Secondary text: labels, hints.
    pub muted: u32,
    /// Tertiary text and inactive things.
    pub dim: u32,
    pub accent: u32,
    pub border: u32,
    pub border_focus: u32,
    pub selected_bg: u32,
    pub ok: u32,
    pub warn: u32,
    pub err: u32,
    pub info: u32,
    /// Text on top of a coloured pill or segment.
    pub on_pill: u32,
    /// The status bar.
    pub bar_bg: u32,
    /// Powerline segments, left to right.
    pub seg: [u32; 4],
    /// The unfilled part of a gauge.
    pub gauge_bg: u32,
    pub magenta: u32,
    pub blue: u32,
}

pub const DEFAULT: &str = "retro-82";

/// The one palette that ships with lmdeck itself; `omp` keeps its original dashboard surfaces.
const OMP_SLUG: &str = "omp";

const OMP: Palette = Palette {
    slug: "omp",
    name: "omp (oh-my-pi)",
    light: false,
    bg: 0x18181e,
    panel: 0x1e1e24,
    panel2: 0x24242c,
    fg: 0xd4d4d4,
    muted: 0x777d88,
    dim: 0x5f6673,
    accent: 0xfebc38,
    border: 0x178fb9,
    border_focus: 0x0088fa,
    selected_bg: 0x31363f,
    ok: 0x89d281,
    warn: 0xe4c00f,
    err: 0xfc3a4b,
    info: 0x00afaf,
    on_pill: 0x121212,
    bar_bg: 0x121212,
    seg: [0xd787af, 0x00afaf, 0x8787af, 0x5faf5f],
    gauge_bg: 0x3d424a,
    magenta: 0xb281d6,
    blue: 0x178fb9,
};

/// WCAG relative luminance of `0xRRGGBB`.
pub fn luminance(c: u32) -> f64 {
    let ch = |v: u32| {
        let s = f64::from(v & 0xff) / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(c >> 16) + 0.7152 * ch(c >> 8) + 0.0722 * ch(c)
}

/// WCAG contrast ratio, 1–21.
pub fn contrast(a: u32, b: u32) -> f64 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// The first candidate with at least `min` contrast against `on`, else the
/// most contrasting one.
fn pick(on: u32, min: f64, cands: &[u32]) -> u32 {
    cands.iter().copied().find(|&c| contrast(c, on) >= min).unwrap_or_else(|| {
        cands.iter().copied().max_by(|a, b| contrast(*a, on).total_cmp(&contrast(*b, on))).unwrap_or(on)
    })
}

impl Palette {
    pub fn from_omarchy(o: &Omarchy) -> Palette {
        let bg = o.background;
        let orange = o.orange.unwrap_or(o.yellow);
        let brown = o.brown.unwrap_or(o.dark_foreground);
        let fg = pick(bg, 4.5, &[o.foreground, o.bright_foreground, o.light_foreground]);
        let accent = pick(bg, 3.0, &[o.accent, o.blue, o.bright_blue, fg]);
        let ok = pick(bg, 3.0, &[o.green, o.bright_green, o.cyan, fg]);
        let warn = pick(bg, 3.0, &[o.yellow, o.bright_yellow, orange, brown, fg]);
        let err = pick(bg, 3.0, &[o.red, o.bright_red, orange, fg]);
        let info = pick(bg, 3.0, &[o.cyan, o.bright_cyan, o.blue, fg]);
        let magenta = pick(bg, 3.0, &[o.magenta, o.bright_magenta, fg]);
        let blue = pick(bg, 3.0, &[o.blue, o.bright_blue, fg]);
        // Pill text: whichever end of the palette reads best on the pill colours.
        let pills = [accent, ok, warn, err, info, magenta, blue];
        let worst = |text: u32| pills.iter().map(|&p| contrast(p, text)).fold(f64::INFINITY, f64::min);
        let on_pill = if worst(bg) >= worst(fg) { bg } else { fg };
        let seg_of = |cands: &[u32]| pick(on_pill, 3.0, cands);
        Palette {
            slug: o.slug,
            name: o.name,
            light: o.light,
            bg,
            panel: o.dark_background,
            panel2: o.lighter_background,
            fg,
            muted: pick(bg, 3.0, &[o.dark_foreground, o.muted, o.light_foreground, fg]),
            dim: pick(bg, 2.0, &[o.muted, o.dark_foreground, o.light_foreground, fg]),
            accent,
            border: pick(bg, 1.6, &[o.muted, o.selection, o.dark_foreground, o.light_foreground]),
            border_focus: pick(bg, 3.0, &[o.accent, o.blue, o.bright_blue, fg]),
            selected_bg: pick(fg, 4.5, &[o.selection, o.lighter_background, o.dark_background, bg]),
            ok,
            warn,
            err,
            info,
            on_pill,
            bar_bg: o.darker_background,
            seg: [
                seg_of(&[o.magenta, o.bright_magenta, accent]),
                seg_of(&[o.cyan, o.bright_cyan, info]),
                seg_of(&[o.blue, o.bright_blue, accent]),
                seg_of(&[o.green, o.bright_green, ok]),
            ],
            gauge_bg: pick(bg, 1.25, &[o.lighter_background, o.selection, o.muted, o.dark_foreground]),
            magenta,
            blue,
        }
    }

    /// Where the colours come from.
    pub fn source(&self) -> String {
        if self.slug == OMP_SLUG {
            "lmdeck (the oh-my-pi palette)".into()
        } else {
            format!("omarchy (github.com/omacom/omarchy) themes/{}/colors.toml", self.slug)
        }
    }

    /// A few representative colours, for pickers.
    pub fn swatch(&self) -> [u32; 8] {
        [self.bg, self.fg, self.accent, self.ok, self.warn, self.err, self.info, self.magenta]
    }

    /// The text colour (background or foreground) that reads best on `c`.
    pub fn on(&self, c: u32) -> u32 {
        if contrast(self.bg, c) >= contrast(self.fg, c) {
            self.bg
        } else {
            self.fg
        }
    }

    /// CSS custom properties for the web dashboard.
    pub fn css_vars(&self) -> Vec<(&'static str, String)> {
        let h = |c: u32| format!("#{c:06x}");
        // The dashboard's original surfaces, kept exactly for the omp theme.
        let (top, page, card, card2, line, line2) = if self.slug == OMP_SLUG {
            (0x121217, 0x18181e, 0x1e1e24, 0x24242c, 0x2e3038, 0x3d424a)
        } else {
            (self.bar_bg, self.bg, self.panel, self.panel2, self.gauge_bg, self.border)
        };
        let fg = if self.slug == OMP_SLUG { 0xe4e4e7 } else { self.fg };
        let muted = if self.slug == OMP_SLUG { 0x9aa0aa } else { self.muted };
        vec![
            ("--bg", h(top)),
            ("--page", h(page)),
            ("--card", h(card)),
            ("--card2", h(card2)),
            ("--line", h(line)),
            ("--line2", h(line2)),
            ("--fg", h(fg)),
            ("--muted", h(muted)),
            ("--dim", h(self.dim)),
            ("--accent", h(self.accent)),
            ("--blue", h(self.blue)),
            ("--cyan", h(self.info)),
            ("--sky", h(self.border_focus)),
            ("--ok", h(self.ok)),
            ("--warn", h(self.warn)),
            ("--err", h(self.err)),
            ("--pink", h(self.seg[0])),
            ("--violet", h(self.seg[2])),
            ("--purple", h(self.magenta)),
            ("--seg1", h(self.seg[0])),
            ("--seg2", h(self.seg[1])),
            ("--seg3", h(self.seg[2])),
            ("--seg4", h(self.seg[3])),
            ("--on-pill", h(self.on_pill)),
            ("--on-accent", h(self.on(self.accent))),
            ("--on-ok", h(self.on(self.ok))),
            ("--on-warn", h(self.on(self.warn))),
            ("--on-info", h(self.on(self.info))),
            ("--shadow", (if self.light { "#0000001f" } else { "#00000066" }).to_string()),
            ("color-scheme", (if self.light { "light" } else { "dark" }).to_string()),
        ]
    }
}

static ALL: LazyLock<Vec<Palette>> =
    LazyLock::new(|| std::iter::once(OMP).chain(OMARCHY.iter().map(Palette::from_omarchy)).collect());

/// Every theme: `omp` first, then Omarchy's in alphabetical order.
pub fn all() -> &'static [Palette] {
    &ALL
}

pub fn default_palette() -> &'static Palette {
    ALL.iter().find(|p| p.slug == DEFAULT).unwrap_or(&ALL[0])
}

/// Lower-case, accents folded, anything else non-alphanumeric as `-`:
/// "Rosé Pine" → `rose-pine`, "Tokyo_Night" → `tokyo-night`.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().to_lowercase().chars() {
        let c = match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            c => c,
        };
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

const ALIASES: &[(&str, &str)] = &[
    ("dark", "omp"),
    ("default", "omp"),
    ("oh-my-pi", "omp"),
    ("mocha", "catppuccin"),
    ("catppuccin-mocha", "catppuccin"),
    ("latte", "catppuccin-latte"),
    ("tokyo", "tokyo-night"),
    ("flexoki", "flexoki-light"),
    ("rose", "rose-pine"),
    ("rose-pine-dawn", "rose-pine"),
    ("osaka", "osaka-jade"),
    ("retro", "retro-82"),
    ("matte", "matte-black"),
];

/// Finds a theme by name: its slug, any spelling that slugifies to it
/// (`Rosé Pine`, `tokyo_night`, `TokyoNight`), an alias (`mocha`, `latte`),
/// or an unambiguous prefix of at least three letters (`kana`).
pub fn index(name: &str) -> Option<usize> {
    let s = slugify(name);
    if s.is_empty() {
        return None;
    }
    let s = ALIASES.iter().find(|(a, _)| *a == s).map(|(_, t)| t.to_string()).unwrap_or(s);
    let all = all();
    if let Some(i) = all.iter().position(|p| p.slug == s) {
        return Some(i);
    }
    let compact = |x: &str| x.replace('-', "");
    if let Some(i) = all.iter().position(|p| compact(p.slug) == compact(&s)) {
        return Some(i);
    }
    if s.len() >= 3 {
        let hits: Vec<usize> = (0..all.len()).filter(|&i| all[i].slug.starts_with(&s)).collect();
        if hits.len() == 1 {
            return Some(hits[0]);
        }
    }
    None
}

pub fn by_name(name: &str) -> Option<&'static Palette> {
    index(name).map(|i| &all()[i])
}

/// "unknown theme 'x'" plus the valid names.
pub fn unknown(name: &str) -> anyhow::Error {
    anyhow::anyhow!("unknown theme '{name}'. Themes: {}", all().iter().map(|p| p.slug).collect::<Vec<_>>().join(", "))
}

/// The theme to use: an explicit choice (`--theme` / `LMDECK_THEME`), else
/// the config file's `theme`, else the default. An unknown explicit name is an
/// error; an unknown name in the config falls back to the default.
pub fn resolve(explicit: Option<&str>, configured: Option<&str>) -> anyhow::Result<&'static Palette> {
    if let Some(n) = explicit.filter(|n| !n.trim().is_empty()) {
        return by_name(n).ok_or_else(|| unknown(n));
    }
    Ok(configured.and_then(by_name).unwrap_or_else(default_palette))
}

/// The name of Omarchy's current theme, if Omarchy is installed:
/// `~/.local/state/omarchy/current/theme.name` (Omarchy 3.x+), else the
/// older `~/.config/omarchy/current/theme` symlink.
pub fn omarchy_current(home: &std::path::Path) -> Option<String> {
    for f in [".local/state/omarchy/current/theme.name", ".config/omarchy/current/theme.name"] {
        if let Ok(s) = std::fs::read_to_string(home.join(f)) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    let link: PathBuf = home.join(".config/omarchy/current/theme");
    let target = std::fs::read_link(&link).ok()?;
    target.file_name().map(|n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_aliases_resolve() {
        assert_eq!(slugify("Rosé Pine"), "rose-pine");
        assert_eq!(slugify("  Tokyo_Night "), "tokyo-night");
        assert_eq!(slugify("Retro 82"), "retro-82");
        let slug = |n: &str| by_name(n).map(|p| p.slug);
        assert_eq!(slug("Rosé Pine"), Some("rose-pine"));
        assert_eq!(slug("ROSE PINE"), Some("rose-pine"));
        assert_eq!(slug("rosepine"), Some("rose-pine"));
        assert_eq!(slug("TokyoNight"), Some("tokyo-night"));
        assert_eq!(slug("tokyo_night"), Some("tokyo-night"));
        assert_eq!(slug("Catppuccin Latte"), Some("catppuccin-latte"));
        assert_eq!(slug("latte"), Some("catppuccin-latte"));
        assert_eq!(slug("mocha"), Some("catppuccin"));
        assert_eq!(slug("Dark"), Some("omp"));
        assert_eq!(slug("kana"), Some("kanagawa"));
        assert_eq!(slug("Matte-Black"), Some("matte-black"));
        assert_eq!(slug("retro82"), Some("retro-82"));
        // Ambiguous prefixes and nonsense don't match.
        assert_eq!(slug("cat"), None);
        assert_eq!(slug("solarized"), None);
        assert_eq!(slug(""), None);
        for p in all() {
            assert_eq!(slug(p.slug), Some(p.slug));
            if p.slug != OMP_SLUG {
                assert_eq!(slug(p.name), Some(p.slug), "display name {}", p.name);
            }
        }
    }

    #[test]
    fn every_omarchy_theme_is_present() {
        let want = [
            "catppuccin",
            "catppuccin-latte",
            "ethereal",
            "everforest",
            "flexoki-light",
            "gruvbox",
            "hackerman",
            "kanagawa",
            "last-horizon",
            "lumon",
            "lupine",
            "matte-black",
            "miasma",
            "nord",
            "osaka-jade",
            "retro-82",
            "ristretto",
            "rose-pine",
            "solitude",
            "tokyo-night",
            "vantablack",
            "white",
        ];
        for w in want {
            assert!(OMARCHY.iter().any(|o| o.slug == w), "{w} missing");
        }
        assert_eq!(all()[0].slug, "omp");
        assert_eq!(all().len(), OMARCHY.len() + 1);
    }

    #[test]
    fn palettes_are_verbatim_omarchy_colours() {
        let nord = OMARCHY.iter().find(|o| o.slug == "nord").unwrap();
        let p = Palette::from_omarchy(nord);
        assert_eq!((p.bg, p.fg, p.accent), (0x2e3440, 0xd8dee9, 0x81a1c1));
        // Every role is one of the theme's own colours.
        for o in OMARCHY {
            let mut own = vec![
                o.accent,
                o.selection,
                o.muted,
                o.background,
                o.dark_background,
                o.darker_background,
                o.lighter_background,
                o.foreground,
                o.dark_foreground,
                o.light_foreground,
                o.bright_foreground,
                o.red,
                o.yellow,
                o.green,
                o.cyan,
                o.blue,
                o.magenta,
                o.bright_red,
                o.bright_yellow,
                o.bright_green,
                o.bright_cyan,
                o.bright_blue,
                o.bright_magenta,
            ];
            own.extend(o.orange);
            own.extend(o.brown);
            let p = Palette::from_omarchy(o);
            let roles = [
                p.bg,
                p.panel,
                p.fg,
                p.muted,
                p.dim,
                p.accent,
                p.border,
                p.border_focus,
                p.selected_bg,
                p.ok,
                p.warn,
                p.err,
                p.info,
                p.on_pill,
                p.bar_bg,
                p.gauge_bg,
                p.magenta,
                p.blue,
            ];
            for c in roles.iter().chain(p.seg.iter()) {
                assert!(own.contains(c), "{}: #{c:06x} is not in its colors.toml", o.slug);
            }
        }
    }

    #[test]
    fn every_theme_is_readable() {
        for p in all() {
            let n = p.slug;
            assert!(contrast(p.fg, p.bg) >= 4.5, "{n}: fg/bg {:.2}", contrast(p.fg, p.bg));
            assert!(contrast(p.fg, p.panel) >= 4.5, "{n}: fg/panel");
            assert!(contrast(p.fg, p.selected_bg) >= 4.5, "{n}: fg/selected {:.2}", contrast(p.fg, p.selected_bg));
            assert!(contrast(p.fg, p.bar_bg) >= 4.5, "{n}: fg/bar");
            assert!(contrast(p.muted, p.bg) >= 3.0, "{n}: muted/bg {:.2}", contrast(p.muted, p.bg));
            assert!(contrast(p.dim, p.bg) >= 2.0, "{n}: dim/bg {:.2}", contrast(p.dim, p.bg));
            for (role, c) in [("accent", p.accent), ("ok", p.ok), ("warn", p.warn), ("err", p.err), ("info", p.info)] {
                assert!(contrast(c, p.bg) >= 3.0, "{n}: {role}/bg {:.2}", contrast(c, p.bg));
            }
            for (i, s) in p.seg.iter().enumerate() {
                assert!(contrast(p.on_pill, *s) >= 3.0, "{n}: seg{i} {:.2}", contrast(p.on_pill, *s));
            }
            assert!(contrast(p.gauge_bg, p.bg) >= 1.1, "{n}: gauge invisible");
            assert_eq!(p.light, luminance(p.bg) > 0.5, "{n}: light flag");
        }
    }

    #[test]
    fn web_variables_are_readable() {
        for p in all() {
            let v: std::collections::HashMap<_, _> = p.css_vars().into_iter().collect();
            let c = |k: &str| u32::from_str_radix(v[k].trim_start_matches('#'), 16).unwrap();
            let n = p.slug;
            for surface in ["--bg", "--page", "--card", "--card2"] {
                assert!(contrast(c("--fg"), c(surface)) >= 4.5, "{n}: fg on {surface}");
            }
            for surface in ["--bg", "--page", "--card"] {
                assert!(contrast(c("--muted"), c(surface)) >= 2.5, "{n}: muted on {surface}");
            }
            for (on, role) in
                [("--on-accent", "--accent"), ("--on-ok", "--ok"), ("--on-warn", "--warn"), ("--on-info", "--cyan")]
            {
                assert!(contrast(c(on), c(role)) >= 3.0, "{n}: {on} on {role}");
            }
            for s in ["--seg1", "--seg2", "--seg3", "--seg4"] {
                assert!(contrast(c("--on-pill"), c(s)) >= 3.0, "{n}: on-pill on {s}");
            }
            assert_eq!(v["color-scheme"], if p.light { "light" } else { "dark" });
        }
    }

    #[test]
    fn resolve_prefers_explicit_then_config() {
        assert_eq!(resolve(None, None).unwrap().slug, "retro-82");
        assert_eq!(default_palette().slug, "retro-82");
        assert_eq!(resolve(None, Some("nord")).unwrap().slug, "nord");
        assert_eq!(resolve(Some("Rosé Pine"), Some("nord")).unwrap().slug, "rose-pine");
        assert_eq!(resolve(None, Some("gone")).unwrap().slug, "retro-82");
        assert!(resolve(Some("gone"), None).unwrap_err().to_string().contains("rose-pine"));
    }

    #[test]
    fn reads_omarchys_current_theme() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        assert_eq!(omarchy_current(d), None);
        std::fs::create_dir_all(d.join(".config/omarchy/current")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/x/themes/osaka-jade", d.join(".config/omarchy/current/theme")).unwrap();
            assert_eq!(omarchy_current(d).as_deref(), Some("osaka-jade"));
        }
        std::fs::create_dir_all(d.join(".local/state/omarchy/current")).unwrap();
        std::fs::write(d.join(".local/state/omarchy/current/theme.name"), "rose-pine\n").unwrap();
        assert_eq!(omarchy_current(d).as_deref(), Some("rose-pine"));
    }
}
