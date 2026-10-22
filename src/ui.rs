//! Terminal output: colours, the wordmark, and tables.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use unicode_width::UnicodeWidthStr;

pub const WORDMARK: &str = include_str!("wordmark.txt");

static COLOR: AtomicBool = AtomicBool::new(false);

/// Enables colour when stdout is a terminal, `NO_COLOR` is unset, and not disabled.
pub fn init_color(disabled: bool) {
    let on = !disabled && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
    COLOR.store(on, Ordering::Relaxed);
}

pub fn color_enabled() -> bool {
    COLOR.load(Ordering::Relaxed)
}

fn paint(code: &str, s: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn cyan(s: &str) -> String {
    paint("36", s)
}

/// The wordmark plus a tagline.
pub fn banner() -> String {
    let mut out = String::new();
    for line in WORDMARK.lines() {
        out.push_str(&cyan(line));
        out.push('\n');
    }
    out.push_str(&dim(&format!("  local LLM deck · v{}", env!("CARGO_PKG_VERSION"))));
    out.push('\n');
    out
}

/// Strips ANSI SGR escape sequences.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Display width, ignoring colour codes.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(strip_ansi(s).as_str())
}

/// Truncates to `max` columns with an ellipsis (colour codes are dropped if cut).
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let plain = strip_ansi(s);
    let mut out = String::new();
    let mut w = 0;
    for c in plain.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// A simple box-drawn table.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    /// Columns right-aligned (numbers).
    right: Vec<usize>,
    /// Max width per column (0 = unlimited).
    max: Vec<usize>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Table {
        Table {
            headers: headers.iter().map(|s| s.to_string()).collect(),
            rows: vec![],
            right: vec![],
            max: vec![0; headers.len()],
        }
    }

    pub fn right(mut self, cols: &[usize]) -> Table {
        self.right = cols.to_vec();
        self
    }

    pub fn max_width(mut self, col: usize, w: usize) -> Table {
        self.max[col] = w;
        self
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn render(&self) -> String {
        let n = self.headers.len();
        let cells: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|r| {
                (0..n)
                    .map(|i| {
                        let c = r.get(i).cloned().unwrap_or_default();
                        if self.max[i] > 0 {
                            truncate(&c, self.max[i])
                        } else {
                            c
                        }
                    })
                    .collect()
            })
            .collect();
        let widths: Vec<usize> = (0..n)
            .map(|i| cells.iter().map(|r| width(&r[i])).chain([width(&self.headers[i])]).max().unwrap_or(0))
            .collect();
        let line = |l: &str, m: &str, r: &str| {
            let mut s = l.to_string();
            for (i, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(w + 2));
                s.push_str(if i + 1 == n { r } else { m });
            }
            s.push('\n');
            s
        };
        let fmt_row = |row: &[String], header: bool| {
            let mut s = "│".to_string();
            for (i, c) in row.iter().enumerate() {
                let pad = " ".repeat(widths[i] - width(c));
                let text = if header { bold(c) } else { c.clone() };
                if self.right.contains(&i) {
                    s.push_str(&format!(" {pad}{text} │"));
                } else {
                    s.push_str(&format!(" {text}{pad} │"));
                }
            }
            s.push('\n');
            s
        };
        let mut out = line("┌", "┬", "┐");
        out.push_str(&fmt_row(&self.headers, true));
        out.push_str(&line("├", "┼", "┤"));
        for r in &cells {
            out.push_str(&fmt_row(r, false));
        }
        out.push_str(&line("└", "┴", "┘"));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_and_measures() {
        assert_eq!(strip_ansi("\x1b[32m●\x1b[0m on"), "● on");
        assert_eq!(width("\x1b[1mabc\x1b[0m"), 3);
        assert_eq!(width("✓ ok"), 4);
        assert_eq!(truncate("abcdefgh", 5), "abcd…");
        assert_eq!(truncate("abc", 5), "abc");
    }

    #[test]
    fn renders_aligned_table() {
        let mut t = Table::new(&["#", "name", "size"]).right(&[0, 2]);
        t.row(vec!["1".into(), "qwen".into(), "2.5G".into()]);
        t.row(vec!["10".into(), "\x1b[32mgpt-oss\x1b[0m".into(), "11G".into()]);
        let out = t.render();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 6);
        let w = width(lines[0]);
        assert!(lines.iter().all(|l| width(l) == w), "{out}");
        assert!(lines[3].starts_with("│  1 │ qwen"));
    }

    #[test]
    fn wordmark_is_six_lines() {
        assert_eq!(WORDMARK.lines().count(), 6);
        assert!(banner().contains("v0.1"));
    }
}
