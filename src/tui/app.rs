//! TUI state and key handling. Pure (no terminal, no I/O) so it can be tested.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::path::PathBuf;
use std::time::Instant;

use crate::browse::{Row as BrowseRow, Sort};
use crate::compat::Level;
use crate::deck::{RecipeView, Snapshot};
use crate::hosts::HostView;
use crate::telemetry::Telemetry;
use crate::tui::theme::{themes, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Deck,
    Hosts,
    Topology,
    Machine,
    Sources,
    Browse,
}

impl Tab {
    pub const ALL: [Tab; 6] = [Tab::Deck, Tab::Hosts, Tab::Topology, Tab::Machine, Tab::Sources, Tab::Browse];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Deck => "Deck",
            Tab::Hosts => "Hosts",
            Tab::Topology => "Topology",
            Tab::Machine => "Machine",
            Tab::Sources => "Sources",
            Tab::Browse => "Browse",
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
    /// The theme picker (`T`): moving previews, ↵ keeps, esc reverts.
    Themes,
    /// The run-on picker (`o`): where to run the selected recipe.
    RunOn,
}

/// One row of the run-on picker: a host and what running there would mean.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOnRow {
    /// `local` or a hosts.toml name.
    pub host: String,
    /// Display name.
    pub label: String,
    /// `local`, `nfs` or `none` (placement mode), or `error`.
    pub mode: String,
    /// Short summary: weights, link, load estimate, memory.
    pub summary: String,
    /// The plan's warnings, or the reason it can't run there.
    pub notes: Vec<String>,
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunOn {
    pub reference: String,
    pub name: String,
    /// Empty while the plans are computed.
    pub rows: Vec<RunOnRow>,
    pub cursor: usize,
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
    /// Pull a wrapped recipe and move a pinned ref to the upstream head.
    Update(String),
    /// Re-snapshot now, re-probing engines too.
    Refresh,
    /// Plan the selected recipe on every host (fills the run-on picker).
    PlanRunOn(String),
    /// Start a recipe on a host (`local` or a hosts.toml name); `only` stops the others there.
    StartOn {
        reference: String,
        host: String,
        only: bool,
    },
    /// Stop a server by name on a host.
    StopOn {
        host: String,
        name: String,
    },
    /// Show a server's log from a host.
    LogsOn {
        host: String,
        name: String,
    },
    /// Persist this theme (a slug) as the config's `theme`.
    SaveTheme(String),
    /// `lmdeck add <name>` for a recipe-index entry.
    Install(String),
    /// Fetch the recipe index now.
    RefreshIndex,
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
    /// Highlighted row in the theme picker.
    pub theme_cursor: usize,
    /// The theme to go back to when the picker is cancelled.
    pub theme_before: usize,
    /// The fabric picture for the Topology tab (built off-thread).
    pub topology: Option<crate::topology::Topology>,
    pub run_on: Option<RunOn>,
    /// Highlighted model of the selected host (Hosts tab).
    pub model_cursor: usize,
    /// The log pane shows this (host, server) instead of the selected recipe's.
    pub remote_log: Option<(String, String)>,
    /// The recipe index (from the cache; refreshed off-thread).
    pub index: Option<crate::index::Loaded>,
    pub index_busy: bool,
    /// Browse tab: search (`tag:x` words filter by tag), cursor, sort, fits only.
    pub browse_query: String,
    pub browse_cursor: usize,
    pub browse_sort: Sort,
    pub browse_fit: bool,
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
            theme_cursor: 0,
            theme_before: 0,
            topology: None,
            run_on: None,
            model_cursor: 0,
            remote_log: None,
            index: None,
            index_busy: false,
            browse_query: String::new(),
            browse_cursor: 0,
            browse_sort: Sort::Name,
            browse_fit: false,
        }
    }
}

impl App {
    pub fn theme(&self) -> &'static Theme {
        &themes()[self.theme % themes().len()]
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

    /// Index entries joined with this machine, before filters.
    pub fn browse_all(&self) -> Vec<BrowseRow> {
        let Some(l) = &self.index else { return vec![] };
        crate::browse::rows(&l.index, self.snap.as_ref(), self.hosts.len().max(1) as u32)
    }

    /// The Browse list after search, `tag:` filters, fits and sort.
    pub fn browse_visible(&self) -> Vec<BrowseRow> {
        let all = self.browse_all();
        let (q, tags) = split_tags(&self.browse_query);
        crate::browse::filter(&all, &q, &tags, self.browse_sort)
            .into_iter()
            .filter(|r| !self.browse_fit || r.fit != Level::Fail)
            .cloned()
            .collect()
    }

    pub fn browse_selected(&self) -> Option<BrowseRow> {
        self.browse_visible().into_iter().nth(self.browse_cursor)
    }

    pub fn set_index(&mut self, l: crate::index::Loaded) {
        let keep = self.browse_selected().map(|r| r.entry.name);
        self.index = Some(l);
        if let Some(i) = keep.and_then(|n| self.browse_visible().iter().position(|r| r.entry.name == n)) {
            self.browse_cursor = i;
        }
        self.clamp();
    }

    fn browse_key(&mut self, k: KeyEvent) -> Option<Action> {
        let action = match k.code {
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('i') => {
                let Some(r) = self.browse_selected() else { return Some(Action::None) };
                if r.installed == "deck" && r.update.is_none() {
                    self.notify(format!("{} is already in your deck", r.entry.name), Level::Ok);
                    return Some(Action::None);
                }
                if r.update.is_some() && r.installed == "deck" {
                    self.notify(r.update.clone().unwrap_or_default(), Level::Warn);
                    return Some(Action::None);
                }
                if self.busy.contains(&r.entry.name) {
                    return Some(Action::None);
                }
                self.busy.push(r.entry.name.clone());
                Action::Install(r.entry.name)
            }
            KeyCode::Char('S') => {
                self.browse_sort = self.browse_sort.next();
                self.notify(format!("sort: {}", self.browse_sort.as_str()), Level::Ok);
                Action::None
            }
            KeyCode::Char('f') => {
                self.browse_fit = !self.browse_fit;
                self.notify(
                    if self.browse_fit { "showing entries that fit" } else { "showing every entry" },
                    Level::Ok,
                );
                Action::None
            }
            KeyCode::Char('r') => {
                if self.index_busy {
                    return Some(Action::None);
                }
                self.index_busy = true;
                self.notify("fetching the recipe index…", Level::Ok);
                Action::RefreshIndex
            }
            _ => return None,
        };
        self.clamp();
        Some(action)
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

    /// `local` or the hosts.toml name of the selected host.
    pub fn selected_host_key(&self) -> Option<String> {
        self.selected_host().map(|h| if h.local { crate::hosts::LOCAL.to_string() } else { h.name.clone() })
    }

    /// Servers running on the selected host (from its telemetry).
    pub fn host_models(&self) -> Vec<String> {
        self.selected_host()
            .and_then(|h| h.telemetry.as_ref())
            .map(|t| t.models.iter().map(|m| m.name.clone()).collect())
            .unwrap_or_default()
    }

    pub fn selected_model(&self) -> Option<String> {
        let m = self.host_models();
        m.get(self.model_cursor.min(m.len().saturating_sub(1))).cloned()
    }

    /// Installs the run-on picker's plans, cursor on the first host that works.
    pub fn set_plans(&mut self, reference: &str, rows: Vec<RunOnRow>) {
        if let Some(p) = self.run_on.as_mut().filter(|p| p.reference == reference) {
            p.cursor = rows.iter().position(|r| r.ok).unwrap_or(0);
            p.rows = rows;
        }
    }

    /// Whether anything on screen is mock data (`LMDECK_DEMO`).
    pub fn demo(&self) -> bool {
        self.hosts.iter().any(|h| h.telemetry.as_ref().is_some_and(|t| t.demo))
    }

    pub fn set_hosts(&mut self, hosts: Vec<HostView>) {
        let keep = self.selected_host().map(|h| h.name.clone());
        self.hosts = hosts;
        if let Some(i) = keep.as_ref().and_then(|n| self.hosts.iter().position(|h| h.name == *n)) {
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
        if self.tab == Tab::Browse {
            let n = self.browse_visible().len();
            self.browse_cursor = if n == 0 { 0 } else { self.browse_cursor.min(n - 1) };
        }
    }

    /// The search box being edited: Browse has its own.
    fn query_mut(&mut self) -> &mut String {
        if self.tab == Tab::Browse {
            &mut self.browse_query
        } else {
            &mut self.query
        }
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
            self.model_cursor = 0;
            return;
        }
        if self.tab == Tab::Browse {
            let n = self.browse_visible().len().max(1) as isize;
            self.browse_cursor = (self.browse_cursor as isize + delta).clamp(0, n - 1) as usize;
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
            Mode::Themes => self.theme_key(k),
            Mode::RunOn => self.run_on_key(k),
            Mode::Normal => self.normal_key(k),
        }
    }

    pub fn open_theme_picker(&mut self) {
        self.theme_before = self.theme;
        self.theme_cursor = self.theme;
        self.mode = Mode::Themes;
    }

    fn theme_key(&mut self, k: KeyEvent) -> Action {
        let n = themes().len() as isize;
        let step = |app: &mut App, d: isize| {
            app.theme_cursor = (app.theme_cursor as isize + d).clamp(0, n - 1) as usize;
            app.theme = app.theme_cursor; // live preview
        };
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => step(self, 1),
            KeyCode::Up | KeyCode::Char('k') => step(self, -1),
            KeyCode::PageDown => step(self, 8),
            KeyCode::PageUp => step(self, -8),
            KeyCode::Home | KeyCode::Char('g') => step(self, -n),
            KeyCode::End | KeyCode::Char('G') => step(self, n),
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.mode = Mode::Normal;
                self.theme = self.theme_cursor;
                return Action::SaveTheme(self.theme().name.to_string());
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('T') => {
                self.theme = self.theme_before;
                self.mode = Mode::Normal;
            }
            _ => {}
        }
        Action::None
    }

    fn run_on_key(&mut self, k: KeyEvent) -> Action {
        let Some(p) = self.run_on.as_mut() else {
            self.mode = Mode::Normal;
            return Action::None;
        };
        let n = p.rows.len().max(1);
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => p.cursor = (p.cursor + 1).min(n - 1),
            KeyCode::Up | KeyCode::Char('k') => p.cursor = p.cursor.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('s') => {
                let only = k.code == KeyCode::Char('s');
                let Some(row) = p.rows.get(p.cursor).cloned() else { return Action::None };
                if !row.ok {
                    let why = row.notes.first().cloned().unwrap_or_else(|| "can't run there".into());
                    self.notify(format!("{}: {why}", row.label), Level::Fail);
                    return Action::None;
                }
                let (reference, name) = (p.reference.clone(), p.name.clone());
                self.run_on = None;
                self.mode = Mode::Normal;
                if !self.busy.contains(&name) {
                    self.busy.push(name);
                }
                return Action::StartOn { reference, host: row.host, only };
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('o') => {
                self.run_on = None;
                self.mode = Mode::Normal;
            }
            _ => {}
        }
        Action::None
    }

    /// Hosts-tab keys: run the deck's selected recipe on the selected host,
    /// stop or tail one of that host's servers.
    fn hosts_key(&mut self, k: KeyEvent) -> Option<Action> {
        let host = self.selected_host_key()?;
        let recipe = self.selected().map(|r| (r.reference.clone(), r.name.clone(), r.nodes.len()));
        let action = match k.code {
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('s') => {
                let Some((reference, name, nodes)) = recipe else {
                    self.notify("pick a recipe on the Deck tab first", Level::Warn);
                    return Some(Action::None);
                };
                if nodes > 0 {
                    let msg =
                        format!("{name} spans {nodes} nodes; its recipe says where each runs (↵ on the Deck tab)");
                    self.notify(msg, Level::Warn);
                    return Some(Action::None);
                }
                if !self.busy.contains(&name) {
                    self.busy.push(name);
                }
                Action::StartOn { reference, host, only: k.code == KeyCode::Char('s') }
            }
            KeyCode::Char('x') => match self.selected_model() {
                Some(name) => Action::StopOn { host, name },
                None => {
                    self.notify("nothing running on this host", Level::Warn);
                    Action::None
                }
            },
            KeyCode::Char('l') => match self.selected_model() {
                Some(name) => {
                    self.remote_log = Some((host.clone(), name.clone()));
                    self.logs.clear();
                    self.logs_max = true;
                    self.log_scroll = 0;
                    self.tab = Tab::Deck;
                    Action::LogsOn { host, name }
                }
                None => {
                    self.notify("nothing running on this host", Level::Warn);
                    Action::None
                }
            },
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('[') => {
                self.model_cursor = self.model_cursor.saturating_sub(1);
                Action::None
            }
            KeyCode::Right | KeyCode::Char(']') => {
                self.model_cursor = (self.model_cursor + 1).min(self.host_models().len().saturating_sub(1));
                Action::None
            }
            _ => return None,
        };
        Some(action)
    }

    fn search_key(&mut self, k: KeyEvent, ctrl: bool) -> Action {
        match k.code {
            KeyCode::Esc => {
                self.query_mut().clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.query_mut().pop();
                self.cursor = 0;
                self.browse_cursor = 0;
            }
            KeyCode::Char('u') if ctrl => {
                self.query_mut().clear();
                self.cursor = 0;
                self.browse_cursor = 0;
            }
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::Char(c) => {
                self.query_mut().push(c);
                self.cursor = 0;
                self.browse_cursor = 0;
            }
            _ => {}
        }
        self.clamp();
        Action::None
    }

    fn normal_key(&mut self, k: KeyEvent) -> Action {
        if self.tab == Tab::Hosts && !self.logs_max {
            if let Some(a) = self.hosts_key(k) {
                return a;
            }
        }
        if self.tab == Tab::Browse && !self.logs_max {
            if let Some(a) = self.browse_key(k) {
                return a;
            }
        }
        match k.code {
            KeyCode::Char('q') => {
                self.quit = true;
                return Action::Quit;
            }
            KeyCode::Esc => {
                if self.logs_max {
                    self.logs_max = false;
                    self.remote_log = None;
                } else if !self.query_mut().is_empty() {
                    self.query_mut().clear();
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
            KeyCode::Char('u') => {
                return self.on_selected(|r| match &r.update {
                    Some(u) if u.available() => Action::Update(r.reference.clone()),
                    _ => Action::None,
                })
            }
            KeyCode::Char('l') => {
                self.logs_max = !self.logs_max;
                self.log_scroll = 0;
                self.remote_log = None;
                self.tab = Tab::Deck;
            }
            KeyCode::Char('o') => {
                if self.tab != Tab::Deck {
                    return Action::None;
                }
                let Some(r) = self.selected() else { return Action::None };
                if !r.nodes.is_empty() {
                    let msg = format!("{} spans {} nodes; its recipe says where each runs", r.name, r.nodes.len());
                    self.notify(msg, Level::Warn);
                    return Action::None;
                }
                let (reference, name) = (r.reference.clone(), r.name.clone());
                self.run_on = Some(RunOn { reference: reference.clone(), name, rows: vec![], cursor: 0 });
                self.mode = Mode::RunOn;
                return Action::PlanRunOn(reference);
            }
            KeyCode::Char('/') => {
                if self.tab != Tab::Browse {
                    self.tab = Tab::Deck;
                }
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
                self.theme = (self.theme + 1) % themes().len();
                self.notify(format!("theme: {}", self.theme().title), Level::Ok);
                return Action::SaveTheme(self.theme().name.to_string());
            }
            KeyCode::Char('T') => self.open_theme_picker(),
            KeyCode::Char('r') => return Action::Refresh,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()],
            KeyCode::BackTab => self.tab = Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()],
            KeyCode::Char(c @ '1'..='6') => self.tab = Tab::ALL[(c as u8 - b'1') as usize],
            _ => {}
        }
        self.clamp();
        Action::None
    }
}

/// `tag:exl3 qwen` → (`qwen`, [`exl3`]); `#exl3` works too.
pub fn split_tags(q: &str) -> (String, Vec<String>) {
    let mut words = vec![];
    let mut tags = vec![];
    for w in q.split_whitespace() {
        match w.strip_prefix("tag:").or_else(|| w.strip_prefix('#')) {
            Some(t) if !t.is_empty() => tags.push(t.to_string()),
            Some(_) => {}
            None => words.push(w),
        }
    }
    (words.join(" "), tags)
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
