//! `lmdeck tui`: a full-screen dashboard (ratatui + crossterm).
//!
//! A worker thread takes snapshots (status probes, disk scans) so the UI
//! thread only draws and handles keys; actions run on their own threads
//! because a stop can wait out a 20 s grace period.

pub mod app;
pub mod hostsview;
pub mod theme;
pub mod view;

use anyhow::Result;
use crossterm::event::{self, Event};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::compat::Level;
use crate::deck::{tail_lines, Deck, Snapshot};
use crate::hosts::{snapshot_once, Sampler};
pub use app::{Action, App, Mode, Tab};

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// An explicit theme (`--theme` / `LMDECK_THEME`); else the config's.
    pub theme: Option<String>,
    pub nerd: bool,
    pub all: bool,
    /// Open with the theme picker up (for `--render-*` screenshots).
    pub picker: bool,
}

enum Msg {
    Snapshot(Box<Snapshot>),
    Done { name: String, result: std::result::Result<String, String> },
}

enum Work {
    Refresh { inventory: bool },
}

type Job = Box<dyn FnOnce(&Deck) -> Result<String> + Send>;

const REFRESH: Duration = Duration::from_secs(2);
const LOG_POLL: Duration = Duration::from_millis(400);

fn initial_app(opts: &Options, deck: &Deck) -> Result<App> {
    let mut app = App { nerd: opts.nerd, show_all: opts.all, ..App::default() };
    let p = crate::theme::resolve(opts.theme.as_deref(), deck.cfg.theme.as_deref())?;
    app.theme = theme::by_name(p.slug).unwrap_or(0);
    if opts.picker {
        app.open_theme_picker();
    }
    Ok(app)
}

pub fn run(deck: Deck, opts: Options) -> Result<()> {
    let deck = Arc::new(deck);
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let (work_tx, work_rx) = mpsc::channel::<Work>();
    {
        let deck = deck.clone();
        let tx = msg_tx.clone();
        std::thread::spawn(move || loop {
            let inventory = match work_rx.recv_timeout(REFRESH) {
                Ok(Work::Refresh { inventory }) => inventory,
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            // Coalesce a burst of refresh requests into one snapshot.
            let inventory = work_rx.try_iter().fold(inventory, |acc, Work::Refresh { inventory }| acc | inventory);
            if tx.send(Msg::Snapshot(Box::new(deck.snapshot(inventory)))).is_err() {
                break;
            }
        });
    }
    let _ = work_tx.send(Work::Refresh { inventory: false });
    // New commits upstream of wrapped recipes (cache only; checks run off-thread).
    crate::updates::spawn_background(deck.paths.clone(), deck.cfg.clone());
    // Telemetry for this machine and every hosts.toml entry, on its own thread.
    let sampler = Sampler::spawn(deck.paths.clone(), true);

    let mut app = initial_app(&opts, &deck)?;
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        let mut last_log = Instant::now() - LOG_POLL;
        let mut last_hosts = Instant::now() - LOG_POLL;
        while !app.quit {
            if last_hosts.elapsed() >= LOG_POLL {
                last_hosts = Instant::now();
                let hosts = sampler.hosts();
                let stamp = |v: &[crate::hosts::HostView]| v.iter().map(|h| h.last_seen).collect::<Vec<_>>();
                if stamp(&hosts) != stamp(&app.hosts) || hosts.len() != app.hosts.len() {
                    app.set_hosts(hosts);
                }
            }
            for m in msg_rx.try_iter() {
                match m {
                    Msg::Snapshot(s) => app.set_snapshot(*s),
                    Msg::Done { name, result } => {
                        app.busy.retain(|n| *n != name);
                        match result {
                            Ok(t) => app.notify(t, Level::Ok),
                            Err(e) => app.notify(e, Level::Fail),
                        }
                    }
                }
            }
            if last_log.elapsed() >= LOG_POLL {
                last_log = Instant::now();
                match app.selected().map(|r| r.name.clone()) {
                    Some(name) => {
                        let path = deck.log_for(&name);
                        app.logs = tail_lines(&path, 500);
                        app.log_path = Some(path);
                    }
                    None => {
                        app.logs.clear();
                        app.log_path = None;
                    }
                }
            }
            terminal.draw(|f| view::draw(f, &app))?;
            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            let Event::Key(k) = event::read()? else { continue };
            let action = app.on_key(k);
            let (name, job): (String, Job) = match action {
                Action::None | Action::Quit => continue,
                Action::Refresh => {
                    app.notify("re-probing engines…", Level::Ok);
                    let _ = work_tx.send(Work::Refresh { inventory: true });
                    continue;
                }
                Action::SaveTheme(slug) => {
                    let title = app.theme().title;
                    match crate::config::Config::save_theme(&deck.paths, Some(&slug)) {
                        Ok(_) if opts.theme.is_some() => app.notify(
                            format!("theme: {title} · saved (--theme / LMDECK_THEME still wins at start)"),
                            Level::Warn,
                        ),
                        Ok(_) => app.notify(format!("theme: {title} · saved"), Level::Ok),
                        Err(e) => app.notify(format!("theme: {title} · not saved: {e:#}"), Level::Fail),
                    }
                    continue;
                }
                Action::Start(r) => (r.clone(), Box::new(move |d| d.start(&r, false, false))),
                Action::Switch(r) => (r.clone(), Box::new(move |d| d.start(&r, true, false))),
                Action::Stop(r) => (r.clone(), Box::new(move |d| d.stop(&r))),
                Action::Pull(r) => (r.clone(), Box::new(move |d| d.pull(&r, false))),
                Action::Update(r) => (r.clone(), Box::new(move |d| d.pull(&r, true))),
            };
            let name = app.selected().map(|r| r.name.clone()).unwrap_or(name);
            let (deck, tx, work) = (deck.clone(), msg_tx.clone(), work_tx.clone());
            std::thread::spawn(move || {
                let result = job(&deck).map_err(|e| format!("{e:#}"));
                let _ = tx.send(Msg::Done { name, result });
                let _ = work.send(Work::Refresh { inventory: false });
            });
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

/// Renders one frame of `app` off-screen.
pub fn render(app: &App, width: u16, height: u16) -> Buffer {
    let mut term = ratatui::Terminal::new(TestBackend::new(width, height)).expect("test backend");
    term.draw(|f| view::draw(f, app)).expect("draw");
    term.backend().buffer().clone()
}

/// One frame of the live deck, for screenshots (`lmdeck tui --render-html`).
pub fn render_snapshot(deck: &Deck, opts: &Options, width: u16, height: u16, tab: Tab) -> Result<Buffer> {
    let mut app = initial_app(opts, deck)?;
    app.tab = tab;
    app.set_snapshot(deck.snapshot(false));
    app.set_hosts(snapshot_once(&deck.paths, true));
    if let Some(name) = app.selected().map(|r| r.name.clone()) {
        let path = deck.log_for(&name);
        app.logs = tail_lines(&path, 500);
        app.log_path = Some(path);
    }
    Ok(render(&app, width, height))
}

/// Plain text of a buffer (one line per row, trailing spaces trimmed).
pub fn buffer_text(buf: &Buffer) -> String {
    let a = buf.area;
    let mut out = String::new();
    for y in 0..a.height {
        let mut line = String::new();
        for x in 0..a.width {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn css(c: Color, default: &str) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Reset => default.to_string(),
        other => {
            let (r, g, b) = match other {
                Color::Black => (0, 0, 0),
                Color::Red => (205, 49, 49),
                Color::Green => (13, 188, 121),
                Color::Yellow => (229, 229, 16),
                Color::Blue => (36, 114, 200),
                Color::Magenta => (188, 63, 188),
                Color::Cyan => (17, 168, 205),
                Color::Gray => (204, 204, 204),
                Color::DarkGray => (118, 118, 118),
                _ => (229, 229, 229),
            };
            format!("#{r:02x}{g:02x}{b:02x}")
        }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A buffer as a standalone HTML page (monospace grid, true colours), used to
/// turn TUI frames into screenshots with a headless browser.
pub fn buffer_html(buf: &Buffer, title: &str) -> String {
    let a = buf.area;
    let mut body = String::new();
    for y in 0..a.height {
        let mut run_style: Option<(String, String, bool, bool)> = None;
        let mut run = String::new();
        let flush = |body: &mut String, st: &Option<(String, String, bool, bool)>, run: &mut String| {
            if let Some((fg, bg, bold, ul)) = st {
                body.push_str(&format!(
                    "<span style=\"color:{fg};background:{bg}{}{}\">{}</span>",
                    if *bold { ";font-weight:700" } else { "" },
                    if *ul { ";text-decoration:underline" } else { "" },
                    html_escape(run)
                ));
            }
            run.clear();
        };
        for x in 0..a.width {
            let c = &buf[(x, y)];
            let st = (
                css(c.fg, "#d4d4d4"),
                css(c.bg, "#18181e"),
                c.modifier.contains(Modifier::BOLD),
                c.modifier.contains(Modifier::UNDERLINED),
            );
            if run_style.as_ref() != Some(&st) {
                flush(&mut body, &run_style, &mut run);
                run_style = Some(st);
            }
            run.push_str(c.symbol());
        }
        flush(&mut body, &run_style, &mut run);
        body.push('\n');
    }
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title><style>\
         html,body{{margin:0;background:#0d0d10}}\
         pre{{margin:0;padding:18px;font:14px/normal 'DejaVu Sans Mono','JetBrains Mono',monospace;\
         display:inline-block;border-radius:10px}}</style></head><body><pre>{body}</pre></body></html>",
        html_escape(title)
    )
}

#[cfg(test)]
mod tests;
