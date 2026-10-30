//! `lmdeck tui`: a full-screen dashboard (ratatui + crossterm).
//!
//! A worker thread takes snapshots (status probes, disk scans) so the UI
//! thread only draws and handles keys; actions run on their own threads
//! because a stop can wait out a 20 s grace period.

pub mod app;
pub mod hostsview;
pub mod theme;
pub mod topoview;
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
pub use app::{Action, App, Mode, RunOnRow, Tab};

/// How often the Topology tab's fabric picture is rebuilt (it pings links).
const TOPO_EVERY: Duration = Duration::from_secs(30);

/// Plans `reference` on this machine and every hosts.toml entry: one picker
/// row each, with the weights home, link, load estimate and fit.
pub fn plan_rows(deck: &Deck, reference: &str) -> Vec<RunOnRow> {
    let node = deck.cfg.node_name();
    let cat = deck.catalog();
    let recipe = match crate::deck::resolve(&cat, reference) {
        Ok(r) => r.recipe.clone(),
        Err(e) => {
            return vec![RunOnRow {
                host: crate::hosts::LOCAL.into(),
                label: node,
                mode: "error".into(),
                summary: String::new(),
                notes: vec![format!("{e:#}")],
                ok: false,
            }]
        }
    };
    let mut keys = vec![crate::hosts::LOCAL.to_string()];
    if let Ok(f) = crate::hosts::HostsFile::load(&deck.paths) {
        keys.extend(f.hosts.iter().map(|h| h.name.clone()));
    }
    keys.into_iter()
        .map(|host| {
            let label = if host == crate::hosts::LOCAL { format!("⌂ {node} (this machine)") } else { host.clone() };
            match crate::placement::plan(&deck.paths, &deck.cfg, &recipe, &host, false) {
                Ok(p) => {
                    let mut parts = vec![match (&p.mode[..], &p.home) {
                        ("local", _) => "weights here".to_string(),
                        ("nfs", Some(h)) => {
                            let h = if h == crate::hosts::LOCAL { node.clone() } else { h.clone() };
                            format!("weights on {h} → NFS ro")
                        }
                        _ => "weights nowhere yet".to_string(),
                    }];
                    if let Some(l) = &p.link {
                        parts.push(l.kind.as_str().to_string());
                    }
                    if let Some(e) = &p.estimate {
                        parts.push(format!("load {}", crate::placement::human_secs(e.secs)));
                    }
                    if let (Some(need), Some(have)) = (p.need_gb, p.run_mem_gb) {
                        parts.push(format!("{need:.0}/{have:.0} GB"));
                    }
                    RunOnRow {
                        host,
                        label,
                        mode: p.mode.clone(),
                        summary: parts.join(" · "),
                        notes: p.warnings,
                        ok: true,
                    }
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    let first = msg.lines().next().unwrap_or("").to_string();
                    RunOnRow {
                        host,
                        label,
                        mode: "error".into(),
                        summary: String::new(),
                        notes: vec![first],
                        ok: false,
                    }
                }
            }
        })
        .collect()
}

/// Logs of a server on `host` (this machine reads its own log file).
fn host_logs(deck: &Deck, host: &str, name: &str) -> std::result::Result<Vec<String>, String> {
    if host == crate::hosts::LOCAL {
        return Ok(tail_lines(&deck.log_for(name), 500));
    }
    let c = crate::remote::Client::named(&deck.paths, host).map_err(|e| format!("{e:#}"))?;
    c.logs(name, 500).map_err(|e| format!("{e:#}"))
}

fn stop_on(deck: &Deck, host: &str, name: &str) -> Result<String> {
    if host == crate::hosts::LOCAL {
        return deck.stop(name);
    }
    let c = crate::remote::Client::named(&deck.paths, host)?;
    c.action("stop", name, false)
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// An explicit theme (`--theme` / `LMDECK_THEME`); else the config's.
    pub theme: Option<String>,
    pub nerd: bool,
    pub all: bool,
    /// Open with the theme picker up (for `--render-*` screenshots).
    pub picker: bool,
    /// For `--render-*`: select this recipe (deck) and/or host (hosts tab).
    pub select: Option<String>,
    pub host: Option<String>,
    /// For `--render-*`: open the run-on picker for the selected recipe.
    pub run_on: bool,
}

enum Msg {
    Snapshot(Box<Snapshot>),
    Done { name: String, result: std::result::Result<String, String> },
    Topology(Box<crate::topology::Topology>),
    Plans { reference: String, rows: Vec<app::RunOnRow> },
    RemoteLogs { host: String, name: String, lines: std::result::Result<Vec<String>, String> },
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
    // The fabric picture (CX7 links, management paths) for the Topology tab.
    {
        let (paths, cfg, tx) = (deck.paths.clone(), deck.cfg.clone(), msg_tx.clone());
        std::thread::spawn(move || loop {
            let views = snapshot_once(&paths, true);
            let t = crate::topology::build(&paths, &cfg, &views, true);
            if tx.send(Msg::Topology(Box::new(t))).is_err() {
                break;
            }
            std::thread::sleep(TOPO_EVERY);
        });
    }

    let mut app = initial_app(&opts, &deck)?;
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        let mut last_log = Instant::now() - LOG_POLL;
        let mut last_hosts = Instant::now() - LOG_POLL;
        let mut remote_inflight = false;
        let mut last_remote = Instant::now() - REFRESH;
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
                    Msg::Topology(t) => app.topology = Some(*t),
                    Msg::Plans { reference, rows } => app.set_plans(&reference, rows),
                    Msg::RemoteLogs { host, name, lines } => {
                        remote_inflight = false;
                        if app.remote_log.as_ref() == Some(&(host.clone(), name.clone())) {
                            match lines {
                                Ok(l) => app.logs = l,
                                Err(e) => app.logs = vec![format!("✗ logs from {host}: {e}")],
                            }
                            app.log_path = None;
                        }
                    }
                }
            }
            if let Some((host, name)) = app.remote_log.clone() {
                // Another host's log: fetched off-thread, one request at a time.
                if !remote_inflight && last_remote.elapsed() >= REFRESH {
                    (remote_inflight, last_remote) = (true, Instant::now());
                    let (deck, tx) = (deck.clone(), msg_tx.clone());
                    std::thread::spawn(move || {
                        let lines = host_logs(&deck, &host, &name);
                        let _ = tx.send(Msg::RemoteLogs { host, name, lines });
                    });
                }
            } else if last_log.elapsed() >= LOG_POLL {
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
                Action::PlanRunOn(reference) => {
                    let (deck, tx) = (deck.clone(), msg_tx.clone());
                    std::thread::spawn(move || {
                        let rows = plan_rows(&deck, &reference);
                        let _ = tx.send(Msg::Plans { reference, rows });
                    });
                    continue;
                }
                Action::LogsOn { .. } => {
                    last_remote = Instant::now() - REFRESH;
                    continue;
                }
                Action::StartOn { reference, host, only } => {
                    let name = app.selected().map(|r| r.name.clone()).unwrap_or_else(|| reference.clone());
                    app.notify(format!("starting {name} on {host}…"), Level::Ok);
                    spawn_job(&deck, &msg_tx, &work_tx, name, move |d| {
                        crate::placement::start_on(d, &reference, &host, only, false, false)
                    });
                    continue;
                }
                Action::StopOn { host, name } => {
                    app.notify(format!("stopping {name} on {host}…"), Level::Ok);
                    spawn_job(&deck, &msg_tx, &work_tx, name.clone(), move |d| stop_on(d, &host, &name));
                    continue;
                }
                Action::Start(r) => (r.clone(), Box::new(move |d| d.start(&r, false, false))),
                Action::Switch(r) => (r.clone(), Box::new(move |d| d.start(&r, true, false))),
                Action::Stop(r) => (r.clone(), Box::new(move |d| d.stop(&r))),
                Action::Pull(r) => (r.clone(), Box::new(move |d| d.pull(&r, false))),
                Action::Update(r) => (r.clone(), Box::new(move |d| d.pull(&r, true))),
            };
            let name = app.selected().map(|r| r.name.clone()).unwrap_or(name);
            spawn_job(&deck, &msg_tx, &work_tx, name, job);
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

/// Runs an action on its own thread; reports back and asks for a refresh.
fn spawn_job(
    deck: &Arc<Deck>,
    tx: &mpsc::Sender<Msg>,
    work: &mpsc::Sender<Work>,
    name: String,
    job: impl FnOnce(&Deck) -> Result<String> + Send + 'static,
) {
    let (deck, tx, work) = (deck.clone(), tx.clone(), work.clone());
    std::thread::spawn(move || {
        let result = job(&deck).map_err(|e| format!("{e:#}"));
        let _ = tx.send(Msg::Done { name, result });
        let _ = work.send(Work::Refresh { inventory: false });
    });
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
    let views = snapshot_once(&deck.paths, true);
    app.set_hosts(views.clone());
    if let Some(want) = &opts.select {
        if let Some(i) = app.visible().iter().position(|r| r.name == *want || r.reference == *want) {
            app.cursor = i;
        }
    }
    if let Some(want) = &opts.host {
        if let Some(i) = app.hosts.iter().position(|h| h.name == *want) {
            app.host_cursor = i;
        }
    }
    if tab == Tab::Topology {
        app.topology = Some(crate::topology::build(&deck.paths, &deck.cfg, &views, true));
    }
    if opts.run_on {
        if let Some(r) = app.selected() {
            let (reference, name) = (r.reference.clone(), r.name.clone());
            let rows = plan_rows(deck, &reference);
            app.run_on = Some(app::RunOn { reference: reference.clone(), name, rows: vec![], cursor: 0 });
            app.mode = Mode::RunOn;
            app.set_plans(&reference, rows);
        }
    }
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

/// Escapes a run of cells, pinning symbols the monospace font may lack (they
/// fall back to a proportional font) to exactly their cell width.
fn pinned(run: &str) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    for ch in run.chars() {
        let plain = ch.is_ascii() || ('\u{2500}'..='\u{259f}').contains(&ch) || ('\u{a0}'..='\u{17f}').contains(&ch);
        let esc = html_escape(&ch.to_string());
        if plain {
            out.push_str(&esc);
        } else {
            let w = ch.width().unwrap_or(1).max(1);
            out.push_str(&format!("<i style=\"width:{w}ch\">{esc}</i>"));
        }
    }
    out
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
                    pinned(run)
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
         pre{{margin:0;padding:18px;font:14px/16px 'DejaVu Sans Mono','JetBrains Mono',monospace;\
         display:inline-block;border-radius:10px}}\
         i{{display:inline-block;font-style:normal;overflow:hidden;vertical-align:top;text-align:center}}</style></head><body><pre>{body}</pre></body></html>",
        html_escape(title)
    )
}

#[cfg(test)]
mod tests;
