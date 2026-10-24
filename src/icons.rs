//! Tiny monochrome device icons as bitmaps, rendered as braille in the TUI
//! and as theme-tinted dot-matrix (SVG circles) on the web.
//!
//! Bitmaps are hand-authored for lmdeck (halftone / braille style). Nothing
//! here is copied from another site.

use serde::Serialize;

/// Device kinds that have a glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IconKind {
    Spark,
    Mac,
    MacStudio,
    Pc,
    Switch,
    Gpu,
    Unknown,
}

impl IconKind {
    pub fn parse(s: &str) -> IconKind {
        match s {
            "spark" => IconKind::Spark,
            "mac" => IconKind::Mac,
            "macstudio" => IconKind::MacStudio,
            "pc" => IconKind::Pc,
            "switch" => IconKind::Switch,
            "gpu" => IconKind::Gpu,
            _ => IconKind::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            IconKind::Spark => "spark",
            IconKind::Mac => "mac",
            IconKind::MacStudio => "macstudio",
            IconKind::Pc => "pc",
            IconKind::Switch => "switch",
            IconKind::Gpu => "gpu",
            IconKind::Unknown => "unknown",
        }
    }
}

/// Fixed 12×8 bitmap (row-major, 1 = lit). Small enough for braille (2×4 per cell)
/// and for SVG circle plots.
pub struct Bitmap {
    pub w: usize,
    pub h: usize,
    pub bits: &'static [u8],
}

impl Bitmap {
    pub fn get(&self, x: usize, y: usize) -> bool {
        x < self.w && y < self.h && self.bits[y * self.w + x] != 0
    }

    /// Braille lines (each cell is 2×4 dots → one U+2800 character).
    pub fn braille(&self) -> Vec<String> {
        let rows = self.h.div_ceil(4);
        let cols = self.w.div_ceil(2);
        let mut out = Vec::with_capacity(rows);
        for r in 0..rows {
            let mut line = String::new();
            for c in 0..cols {
                let mut bits = 0u8;
                // Braille dot order: 1 4 / 2 5 / 3 6 / 7 8
                let map = [(0, 0, 0), (0, 1, 1), (0, 2, 2), (1, 0, 3), (1, 1, 4), (1, 2, 5), (0, 3, 6), (1, 3, 7)];
                for (dx, dy, bit) in map {
                    if self.get(c * 2 + dx, r * 4 + dy) {
                        bits |= 1 << bit;
                    }
                }
                line.push(char::from_u32(0x2800 + bits as u32).unwrap_or('⠀'));
            }
            out.push(line);
        }
        out
    }

    /// Lit pixel coordinates centred on (0,0) for SVG / canvas.
    pub fn dots(&self) -> Vec<(i32, i32)> {
        let mut v = vec![];
        let ox = self.w as i32 / 2;
        let oy = self.h as i32 / 2;
        for y in 0..self.h {
            for x in 0..self.w {
                if self.get(x, y) {
                    v.push((x as i32 - ox, y as i32 - oy));
                }
            }
        }
        v
    }
}

// 12×8 silhouettes. 1 = lit. Hand-drawn; intentionally chunky.
// DGX Spark: square chassis with a front grille of vertical bars.
const SPARK: &[u8] = &[
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // 0
    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, // 1
    1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 0, 1, // 2 grille
    1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 0, 1, // 3
    1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 0, 1, // 4
    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, // 5
    1, 0, 0, 0, 1, 1, 1, 0, 0, 0, 0, 1, // 6 LED bar
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // 7
];
// MacBook: thin wedge with a lit display.
const MAC: &[u8] = &[
    0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 1, 1, 1, 1, 1, 1, 0, 1, 0, 0, 1,
    0, 1, 1, 1, 1, 1, 1, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 1,
    1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 0, 0,
];
// Mac Studio: squat cylinder / box with top vent dots.
const MACSTUDIO: &[u8] = &[
    0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1,
    0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1, 0, 0,
    0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0,
];
// Generic PC / tower.
const PC: &[u8] = &[
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 1, 1, 1, 0, 0, 0, 0, 1, 0, 0, 1,
    0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 1, 1, 1, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0,
    0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0,
];
// Switch: wide 1U with port LEDs.
const SWITCH: &[u8] = &[
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 1, 1, 0, 1, 1, 0, 1, 1, 0, 1, 1, 0,
    1, 1, 0, 1, 1, 0, 1, 1, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 0, 1, 1, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
];
// GPU card: PCIe board with dual fans.
const GPU: &[u8] = &[
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 1, 0, 0, 0, 1, 1, 0, 0, 1, 1, 0,
    1, 1, 0, 0, 0, 1, 1, 0, 0, 1, 1, 0, 1, 1, 0, 0, 0, 1, 1, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];
const UNKNOWN: &[u8] = &[
    0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 1, 1, 0, 0, 0, 1, 0, 0, 1,
    0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0,
    0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0,
];

pub fn bitmap(kind: IconKind) -> Bitmap {
    let bits = match kind {
        IconKind::Spark => SPARK,
        IconKind::Mac => MAC,
        IconKind::MacStudio => MACSTUDIO,
        IconKind::Pc => PC,
        IconKind::Switch => SWITCH,
        IconKind::Gpu => GPU,
        IconKind::Unknown => UNKNOWN,
    };
    Bitmap { w: 12, h: 8, bits }
}

/// JSON payload the web dashboard embeds once: kind → list of [x,y] dots.
pub fn catalog_json() -> serde_json::Value {
    use serde_json::json;
    let mut map = serde_json::Map::new();
    for k in [
        IconKind::Spark,
        IconKind::Mac,
        IconKind::MacStudio,
        IconKind::Pc,
        IconKind::Switch,
        IconKind::Gpu,
        IconKind::Unknown,
    ] {
        let dots: Vec<serde_json::Value> = bitmap(k).dots().into_iter().map(|(x, y)| json!([x, y])).collect();
        map.insert(k.as_str().into(), json!(dots));
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braille_is_two_rows_of_six() {
        let b = bitmap(IconKind::Spark).braille();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].chars().count(), 6);
        assert!(b[0].chars().all(|c| ('\u{2800}'..='\u{28FF}').contains(&c)));
    }

    #[test]
    fn every_kind_has_lit_pixels() {
        for k in [
            IconKind::Spark,
            IconKind::Mac,
            IconKind::MacStudio,
            IconKind::Pc,
            IconKind::Switch,
            IconKind::Gpu,
            IconKind::Unknown,
        ] {
            assert!(!bitmap(k).dots().is_empty(), "{k:?}");
        }
    }
}
