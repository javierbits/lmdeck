//! TUI state and key handling. Pure (no terminal, no I/O) so it can be tested.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::path::PathBuf;
use std::time::Instant;

use crate::compat::Level;
use crate::deck::{RecipeView, Snapshot};
use crate::hosts::HostView;
use crate::telemetry::Telemetry;
use crate::tui::theme::{Theme, THEMES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Deck,
    Hosts,
    Machine,
    Sources,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Deck, Tab::Hosts, Tab::Machine, Tab::Sources];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Deck => "Deck",
            Tab::Hosts => "Hosts",
            Tab::Machine => "Machine",
            Tab::Sources => "Sources",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search,
    Help,
}

/// Something the runtime has to do on the app's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Start(String),
    Stop(String),
    Switch(String),
    Pull(String),
    /// Re-snapshot now, re-probing engines too.
    Refresh,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub level: Level,
    pub at: Instant,
}

#[derive(Debug, Clone)]
pub struct App {
    pub snap: Option<Snapshot>,
    pub tab: Tab,
    pub mode: Mode,
    pub cursor: usize,
    pub query: String,
    pub only_fit: bool,
    pub show_all: bool,
    pub theme: usize,
    /// Logs pane takes over the right-hand column.
    pub logs_max: bool,
    /// Lines scrolled up from the bottom of the log.
    pub log_scroll: usize,
    pub logs: Vec<String>,
    pub log_path: Option<PathBuf>,
    pub toast: Option<Toast>,
    /// Recipes with an action in flight.
    pub busy: Vec<String>,
    /// Use Nerd Font powerline glyphs.
    pub nerd: bool,
    pub quit: bool,
    /// This machine first, then every `hosts.toml` entry.
    pub hosts: Vec<HostView>,
    pub host_cursor: usize,
}

impl Default for App {
    fn default() -> Self {
        App {
            snap: None,
            tab: Tab::Deck,
            mode: Mode::Normal,
            cursor: 0,
            query: String::new(),
            only_fit: false,
            show_all: false,
            theme: 0,
            logs_max: false,
            log_scroll: 0,
            logs: vec![],
            log_path: None,
            toast: None,
            busy: vec![],
            nerd: false,
            quit: false,
            hosts: vec![],
            host_cursor: 0,
        }
    }
}

impl App {
    pub fn theme(&self) -> &'static Theme {
        &THEMES[self.theme % THEMES.len()]
    }

    /// Recipes after the deck/all, compatible and fuzzy filters; best match first.
    pub fn visible(&self) -> Vec<&RecipeView> {
        let Some(s) = &self.snap else { return vec![] };
        let mut v: Vec<(i64, &RecipeView)> = s
            .recipes
            .iter()
            .filter(|r| self.show_all || r.listed)
            .filter(|r| !self.only_fit || r.fits())
            .filter_map(|r| {
                if self.query.trim().is_empty() {
                    return Some((0, r));
                }
                let hay = format!(
                    "{} {} {} {} {} {} {}",
                    r.name,
                    r.display_name,
                    r.engine,
                    r.quant,
                    r.model.as_deref().unwrap_or(""),
                    r.tags.join(" "),
                    r.origin
                );
                fuzzy_score(&self.query, &hay).map(|sc| (sc, r))
            })
            .collect();
        if !self.query.trim().is_empty() {
            v.sort_by_key(|x| std::cmp::Reverse(x.0));
        }
        v.into_iter().map(|(_, r)| r).collect()
    }

    pub fn selected(&self) -> Option<&RecipeView> {
        self.visible().get(self.cursor).copied()
    }

    /// Installs a fresh snapshot, keeping the same recipe selected.
    pub fn set_snapshot(&mut self, s: Snapshot) {
        let first = self.snap.is_none();
        let keep = self.selected().map(|r| r.reference.clone());
        // Forget in-flight markers once the state they were waiting for shows up.
        self.busy.retain(|name| {
            let old = self.snap.as_ref().and_then(|o| o.find(name)).map(|r| (r.status.clone(), r.pulling));
            let new = s.find(name).map(|r| (r.status.clone(), r.pulling));
            old == new && new.is_some()
        });
        self.snap = Some(s);
        if let Some(reference) = keep {
            if let Some(i) = self.visible().iter().position(|r| r.reference == reference) {
                self.cursor = i;
            }
        } else if first {
            // Open on whatever is serving, so its logs are the first thing you see.
            self.cursor = self.visible().iter().position(|r| r.is_up()).unwrap_or(0);
        }
        self.clamp();
    }

    /// Live telemetry of this machine, once the sampler has produced some.
    pub fn local(&self) -> Option<&Telemetry> {
        self.hosts.iter().find(|h| h.local).and_then(|h| h.telemetry.as_ref())
    }

    pub fn selected_host(&self) -> Option<&HostView> {
        self.hosts.get(self.host_cursor)
    }

    /// Whether anything on screen is mock data (`LMDECK_DEMO`).
    pub fn demo(&self) -> bool {
        self.hosts.iter().any(|h| h.telemetry.as_ref().is_some_and(|t| t.demo))
    }

    pub fn set_hosts(&mut self, hosts: Vec<HostView>) {
        let keep = self.selected_host().map(|h| h.name.clone());
        self.hosts = hosts;
        if let Some(i) = keep.and_then(|n| self.hosts.iter().position(|h| h.name == n)) {
            self.host_cursor = i;
        }
        self.host_cursor = self.host_cursor.min(self.hosts.len().saturating_sub(1));
    }

    pub fn notify(&mut self, text: impl Into<String>, level: Level) {
        self.toast = Some(Toast { text: text.into(), level, at: Instant::now() });
    }

    fn clamp(&mut self) {
        let n = self.visible().len();
        self.cursor = if n == 0 { 0 } else { self.cursor.min(n - 1) };
    }

    fn move_by(&mut self, delta: isize) {
        if self.logs_max {
            let max = self.logs.len().saturating_sub(1);
            self.log_scroll = (self.log_scroll as isize - delta).clamp(0, max as isize) as usize;
            return;
        }
        if self.tab == Tab::Hosts {
            let n = self.hosts.len().max(1) as isize;
            self.host_cursor = (self.host_cursor as isize + delta).clamp(0, n - 1) as usize;
            return;
        }
        let n = self.visible().len();
        if n == 0 {
            return;
        }
        let c = (self.cursor as isize + delta).clamp(0, n as isize - 1) as usize;
        if c != self.cursor {
            self.cursor = c;
            self.log_scroll = 0;
        }
    }

    fn on_selected(&mut self, f: impl Fn(&RecipeView) -> Action) -> Action {
        if self.tab != Tab::Deck {
            return Action::None;
        }
        match self.selected() {
            Some(r) => {
                let a = f(r);
                let name = r.name.clone();
                if a != Action::None && !self.busy.contains(&name) {
                    self.busy.push(name);
                }
                a
            }
            None => Action::None,
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Action {
        if k.kind == KeyEventKind::Release {
            return Action::None;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.quit = true;
            return Action::Quit;
        }
        match self.mode {
            Mode::Help => {
                self.mode = Mode::Normal;
                Action::None
            }
            Mode::Search => self.search_key(k, ctrl),
            Mode::Normal => self.normal_key(k),
        }
    }

    fn search_key(&mut self, k: KeyEvent, ctrl: bool) -> Action {
        match k.code {
            KeyCode::Esc => {
                self.query.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.query.pop();
                self.cursor = 0;
            }
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.cursor = 0;
            }
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::Char(c) => {
                self.query.push(c);
                self.cursor = 0;
            }
            _ => {}
        }
        self.clamp();
        Action::None
    }

    fn normal_key(&mut self, k: KeyEvent) -> Action {
        match k.code {
            KeyCode::Char('q') => {
                self.quit = true;
                return Action::Quit;
            }
            KeyCode::Esc => {
                if self.logs_max {
                    self.logs_max = false;
                } else if !self.query.is_empty() {
                    self.query.clear();
                } else {
                    self.quit = true;
                    return Action::Quit;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(-10_000),
            KeyCode::End | KeyCode::Char('G') => self.move_by(10_000),
            KeyCode::Enter | KeyCode::Char(' ') => {
                return self.on_selected(|r| {
                    if r.is_up() {
                        Action::Stop(r.reference.clone())
                    } else {
                        Action::Start(r.reference.clone())
                    }
                })
            }
            KeyCode::Char('s') => return self.on_selected(|r| Action::Switch(r.reference.clone())),
            KeyCode::Char('x') => {
                return self.on_selected(|r| if r.is_up() { Action::Stop(r.reference.clone()) } else { Action::None })
            }
            KeyCode::Char('p') => {
                return self.on_selected(|r| if r.pulling { Action::None } else { Action::Pull(r.reference.clone()) })
            }
            KeyCode::Char('l') => {
                self.logs_max = !self.logs_max;
                self.log_scroll = 0;
                self.tab = Tab::Deck;
            }
            KeyCode::Char('/') => {
                self.tab = Tab::Deck;
                self.mode = Mode::Search;
            }
            KeyCode::Char('f') => {
                self.only_fit = !self.only_fit;
                self.notify(if self.only_fit { "showing recipes that fit" } else { "showing all fits" }, Level::Ok);
            }
            KeyCode::Char('a') => {
                self.show_all = !self.show_all;
                self.notify(if self.show_all { "every known recipe" } else { "deck only" }, Level::Ok);
            }
            KeyCode::Char('t') => {
                self.theme = (self.theme + 1) % THEMES.len();
                self.notify(format!("theme: {}", self.theme().name), Level::Ok);
            }
            KeyCode::Char('r') => return Action::Refresh,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()],
            KeyCode::BackTab => self.tab = Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()],
            KeyCode::Char(c @ '1'..='4') => self.tab = Tab::ALL[(c as u8 - b'1') as usize],
            _ => {}
        }
        self.clamp();
        Action::None
    }
}

/// Subsequence fuzzy match; every whitespace-separated word of `query` has to
/// match. Higher is better; consecutive runs and word starts score extra.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i64> {
    let hay: Vec<char> = text.to_lowercase().chars().collect();
    let mut total = 0;
    for word in query.split_whitespace() {
        let mut score = 0i64;
        let mut pos = 0usize;
        let mut prev: Option<usize> = None;
        for qc in word.to_lowercase().chars() {
            let i = (pos..hay.len()).find(|&i| hay[i] == qc)?;
            score += 1;
            match prev {
                Some(p) if p + 1 == i => score += 6,
                Some(p) => score -= (i - p - 1).min(10) as i64,
                None if i == 0 || !hay[i - 1].is_alphanumeric() => score += 8,
                None => {}
            }
            prev = Some(i);
            pos = i + 1;
        }
        total += score;
    }
    Some(total)
}
