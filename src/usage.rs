//! Local usage statistics: tokens each model produced, by day and hour.
//!
//! Opt-in: recording only happens with `[stats] record = true` in config.toml
//! (the first-run wizard asks, default off). It is fed by the telemetry
//! sampler, which already scrapes every local engine's Prometheus `/metrics`
//! for live tok/s; the same counters (generated and prompt tokens, plus a
//! request counter where the engine has one) are sampled and their deltas are
//! accumulated here. No Prometheus server, no network, nothing leaves the
//! machine.
//!
//! Storage is `<state>/usage.json` (`~/.local/state/lmdeck` by default),
//! written atomically and pruned to [`KEEP_DAYS`]. Only one process records:
//! a `usage.lock` held with `flock` keeps the TUI and `lmdeck serve` (or two
//! TUIs) from counting the same tokens twice.
//!
//! Counters reset when a server restarts. A drop rebases the baseline and
//! counts the new process's current total; the first sample of a model (fresh
//! sampler or fresh process) is only a baseline, so restarting lmdeck never
//! double counts.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::paths::Paths;
use crate::telemetry::Telemetry;
use crate::util::now_secs;

/// File format version.
pub const SCHEMA: u32 = 1;
/// Days kept in the store (the heatmap wants a year and change).
pub const KEEP_DAYS: i64 = 400;
/// How often accumulated counts are written while recording.
const FLUSH_EVERY: Duration = Duration::from_secs(5);

/// Token and request counters for one model in one place/time bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    #[serde(default)]
    pub gen: u64,
    #[serde(default)]
    pub prompt: u64,
    #[serde(default)]
    pub requests: u64,
}

impl Counts {
    pub fn tokens(&self) -> u64 {
        self.gen.saturating_add(self.prompt)
    }

    fn add(&mut self, other: Counts) {
        self.gen = self.gen.saturating_add(other.gen);
        self.prompt = self.prompt.saturating_add(other.prompt);
        self.requests = self.requests.saturating_add(other.requests);
    }

    fn is_zero(&self) -> bool {
        self.gen == 0 && self.prompt == 0 && self.requests == 0
    }
}

/// `usage.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default = "schema")]
    pub schema: u32,
    /// `YYYY-MM-DD` → the day's hours.
    #[serde(default)]
    pub days: BTreeMap<String, Day>,
}

fn schema() -> u32 {
    SCHEMA
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Day {
    /// Local hour (0-23) → model name → counters.
    #[serde(default)]
    pub hours: BTreeMap<u8, Hour>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Hour {
    #[serde(default)]
    pub models: BTreeMap<String, Counts>,
}

pub fn store_path(paths: &Paths) -> PathBuf {
    paths.state_dir.join("usage.json")
}

fn lock_path(paths: &Paths) -> PathBuf {
    paths.state_dir.join("usage.lock")
}

impl Store {
    pub fn load(path: &Path) -> Store {
        let Ok(text) = fs::read_to_string(path) else { return Store::default() };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Writes atomically, dropping days outside the retention window.
    fn save(&mut self, path: &Path, today: i64) -> std::io::Result<()> {
        self.schema = SCHEMA;
        let oldest = date_key(today.saturating_sub(KEEP_DAYS));
        self.days.retain(|k, _| *k >= oldest);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        fs::write(&tmp, body)?;
        fs::rename(&tmp, path)
    }
}

/// Holds the record lock for its lifetime; dropping it lets another
/// front-end take over.
pub struct Recorder {
    enabled: bool,
    _lock: Option<File>,
    path: PathBuf,
    store: Store,
    prev: HashMap<String, Prev>,
    dirty: bool,
    last_flush: Instant,
}

#[derive(Clone, Copy, Default)]
struct Prev {
    gen: Option<u64>,
    prompt: Option<u64>,
    requests: Option<u64>,
}

impl Recorder {
    /// A recorder for this machine. With `enabled`, it takes the file lock;
    /// if another process holds it, this one stays quiet (that process is
    /// already recording).
    pub fn new(paths: &Paths, enabled: bool) -> Recorder {
        let mut rec = Recorder {
            enabled: false,
            _lock: None,
            path: store_path(paths),
            store: Store::default(),
            prev: HashMap::new(),
            dirty: false,
            last_flush: Instant::now(),
        };
        if enabled {
            if let Some(lock) = take_lock(paths) {
                rec.store = Store::load(&rec.path);
                rec.enabled = true;
                rec._lock = Some(lock);
            }
        }
        rec
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Adds this sample's counter deltas, and writes now and then.
    pub fn observe(&mut self, tel: &Telemetry) {
        if !self.enabled || tel.models.is_empty() {
            return;
        }
        let (day, hour) = local_day_hour(now_secs());
        let mut live: Vec<&str> = vec![];
        for m in &tel.models {
            let Some(gen) = counter(m.tokens_total) else { continue };
            live.push(m.name.as_str());
            let cur =
                Prev { gen: Some(gen), prompt: counter(m.prompt_tokens_total), requests: counter(m.requests_total) };
            let Some(prev) = self.prev.get_mut(&m.name) else {
                // First sight: whatever it already produced happened before we
                // watched, so it is only a baseline.
                self.prev.insert(m.name.clone(), cur);
                continue;
            };
            let (dg, gb) = field_delta(prev.gen, cur.gen);
            let (dp, pb) = field_delta(prev.prompt, cur.prompt);
            let (dr, rb) = field_delta(prev.requests, cur.requests);
            prev.gen = gb;
            prev.prompt = pb;
            prev.requests = rb;
            let d = Counts { gen: dg, prompt: dp, requests: dr };
            if d.is_zero() {
                continue;
            }
            let h = self.store.days.entry(date_key(day)).or_default().hours.entry(hour).or_default();
            h.models.entry(m.name.clone()).or_default().add(d);
            self.dirty = true;
        }
        self.prev.retain(|name, _| live.contains(&name.as_str()));
        if self.dirty && self.last_flush.elapsed() >= FLUSH_EVERY {
            self.flush(true);
        }
    }

    /// Writes accumulated counts. `force` (or the periodic flush) writes;
    /// errors are silent — statistics never fail a run.
    pub fn flush(&mut self, force: bool) {
        if !self.enabled || !self.dirty {
            return;
        }
        if !force && self.last_flush.elapsed() < FLUSH_EVERY {
            return;
        }
        let today = local_day_hour(now_secs()).0;
        self.store.save(&self.path, today).ok();
        self.dirty = false;
        self.last_flush = Instant::now();
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.flush(true);
    }
}

/// `(added, new baseline)` for one monotonic counter. A drop means the server
/// restarted: the new process's current value is what it has produced since,
/// so it is counted and becomes the baseline. A value that is absent now
/// keeps the old baseline; one that appears for the first time (no previous
/// value) only sets a baseline.
fn field_delta(prev: Option<u64>, cur: Option<u64>) -> (u64, Option<u64>) {
    match (prev, cur) {
        (_, None) => (0, prev),
        (None, Some(c)) => (0, Some(c)),
        (Some(p), Some(c)) if c >= p => (c - p, Some(c)),
        (Some(_), Some(c)) => (c, Some(c)),
    }
}

fn counter(v: Option<f64>) -> Option<u64> {
    v.filter(|x| x.is_finite() && *x >= 0.0).map(|x| x.round() as u64)
}

/// Takes `<state>/usage.lock` without blocking. `None` when another process
/// records for this machine.
fn take_lock(paths: &Paths) -> Option<File> {
    fs::create_dir_all(&paths.state_dir).ok()?;
    let f = OpenOptions::new().create(true).write(true).truncate(false).open(lock_path(paths)).ok()?;
    use std::os::unix::io::AsRawFd;
    // SAFETY: flock on an fd the File owns; released when the File drops.
    let ok = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    ok.then_some(f)
}

// ───────────────────────── dates ─────────────────────────

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD`.
pub fn date_key(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Parses `YYYY-MM-DD` back to a day number.
pub fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// 0 = Sunday, matching the calendar layout.
pub fn weekday(day: i64) -> usize {
    (((day + 4) % 7 + 7) % 7) as usize
}

/// The local date and hour now (`localtime_r`, so DST and the machine's zone
/// apply). Falls back to UTC if the C library refuses.
pub fn local_day_hour(secs: u64) -> (i64, u8) {
    // SAFETY: localtime_r fills a caller-owned tm from a time_t; no globals.
    unsafe {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return ((secs / 86_400) as i64, ((secs / 3600) % 24) as u8);
        }
        let day = days_from_civil(tm.tm_year as i64 + 1900, (tm.tm_mon + 1) as u32, tm.tm_mday as u32);
        (day, tm.tm_hour.clamp(0, 23) as u8)
    }
}

pub fn today() -> i64 {
    local_day_hour(now_secs()).0
}

// ───────────────────────── view ─────────────────────────

/// One day as the Stats tab and JSON API show it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DayView {
    pub day: i64,
    pub gen: u64,
    pub prompt: u64,
    pub requests: u64,
    /// Tokens per local hour (generated + prompt).
    pub hours: [u64; 24],
    pub models: BTreeMap<String, Counts>,
}

impl DayView {
    pub fn tokens(&self) -> u64 {
        self.gen.saturating_add(self.prompt)
    }

    fn merge(&mut self, other: &DayView) {
        self.gen = self.gen.saturating_add(other.gen);
        self.prompt = self.prompt.saturating_add(other.prompt);
        self.requests = self.requests.saturating_add(other.requests);
        for (a, b) in self.hours.iter_mut().zip(&other.hours) {
            *a = a.saturating_add(*b);
        }
        for (name, c) in &other.models {
            self.models.entry(name.clone()).or_default().add(*c);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelUse {
    pub name: String,
    pub gen: u64,
    pub prompt: u64,
    pub requests: u64,
    pub days: u64,
    pub last_day: i64,
}

impl ModelUse {
    pub fn tokens(&self) -> u64 {
        self.gen.saturating_add(self.prompt)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DayTotal {
    pub day: i64,
    pub tokens: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HourUse {
    pub hour: u8,
    pub tokens: u64,
}

/// Everything the Stats tab draws, aggregated from the store. `recording`
/// reflects the config, not this process's lock.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageView {
    pub recording: bool,
    pub today: i64,
    /// Oldest first, within the retention window.
    pub days: Vec<DayView>,
    pub total: Counts,
    pub today_counts: Counts,
    pub last7: Counts,
    pub models: Vec<ModelUse>,
    pub active_days: u64,
    /// Days with activity ending today (or yesterday, while today is still
    /// empty); 0 when the last active day is older.
    pub streak_days: u64,
    pub best_day: Option<DayTotal>,
    pub peak_hour: Option<HourUse>,
    /// Tokens per local hour over the whole window.
    pub all_hours: [u64; 24],
}

impl UsageView {
    pub fn has_data(&self) -> bool {
        !self.days.is_empty()
    }

    pub fn day(&self, day: i64) -> Option<&DayView> {
        self.days.iter().find(|d| d.day == day)
    }

    /// Adds another host's usage: days and counters are summed, then every
    /// aggregate is recomputed. Each host bucketed by its own local clock, so
    /// hosts in different timezones stay on their own day/hour keys.
    pub fn merge(&mut self, other: &UsageView) {
        for od in &other.days {
            match self.days.iter_mut().find(|d| d.day == od.day) {
                Some(d) => d.merge(od),
                None => self.days.push(od.clone()),
            }
        }
        let today = self.today;
        let recording = self.recording || other.recording;
        let days = std::mem::take(&mut self.days);
        *self = aggregate(days, recording, today);
    }
}

/// Reads and aggregates the store.
pub fn view(paths: &Paths, recording: bool) -> UsageView {
    let store = Store::load(&store_path(paths));
    let now = today();
    let oldest = now.saturating_sub(KEEP_DAYS);
    let mut days = vec![];
    for (key, day) in &store.days {
        let Some(d) = parse_date(key) else { continue };
        if d > now || d < oldest {
            continue;
        }
        let mut dv = DayView { day: d, ..Default::default() };
        for (hour, h) in &day.hours {
            if *hour > 23 {
                continue;
            }
            for (name, c) in &h.models {
                dv.gen = dv.gen.saturating_add(c.gen);
                dv.prompt = dv.prompt.saturating_add(c.prompt);
                dv.requests = dv.requests.saturating_add(c.requests);
                dv.hours[*hour as usize] = dv.hours[*hour as usize].saturating_add(c.tokens());
                dv.models.entry(name.clone()).or_default().add(*c);
            }
        }
        days.push(dv);
    }
    aggregate(days, recording, now)
}

/// Every figure the Stats tab shows, recomputed from a day list. Used for one
/// host's store and after merging a fleet.
fn aggregate(days: Vec<DayView>, recording: bool, today: i64) -> UsageView {
    let mut days: Vec<DayView> = days
        .into_iter()
        .filter(|d| d.tokens() > 0 && d.day <= today && d.day >= today.saturating_sub(KEEP_DAYS))
        .collect();
    days.sort_by_key(|d| d.day);
    let mut v = UsageView { recording, today, ..Default::default() };
    let mut models: BTreeMap<String, ModelUse> = BTreeMap::new();
    for d in &days {
        let total = Counts { gen: d.gen, prompt: d.prompt, requests: d.requests };
        v.active_days += 1;
        v.total.add(total);
        if d.day == today {
            v.today_counts.add(total);
        }
        if d.day > today - 7 {
            v.last7.add(total);
        }
        for (hour, tokens) in d.hours.iter().enumerate() {
            v.all_hours[hour] = v.all_hours[hour].saturating_add(*tokens);
        }
        for (name, c) in &d.models {
            let m = models.entry(name.clone()).or_insert_with(|| ModelUse { name: name.clone(), ..Default::default() });
            m.gen = m.gen.saturating_add(c.gen);
            m.prompt = m.prompt.saturating_add(c.prompt);
            m.requests = m.requests.saturating_add(c.requests);
            m.days += 1;
            m.last_day = m.last_day.max(d.day);
        }
    }
    if let Some(best) = days.iter().max_by_key(|d| d.tokens()) {
        v.best_day = Some(DayTotal { day: best.day, tokens: best.tokens() });
    }
    if let Some((hour, tokens)) = v.all_hours.iter().enumerate().max_by_key(|(_, t)| **t).filter(|(_, t)| **t > 0) {
        v.peak_hour = Some(HourUse { hour: hour as u8, tokens: *tokens });
    }
    // Streak: today, or yesterday when today hasn't seen tokens yet.
    let active: std::collections::BTreeSet<i64> = days.iter().map(|d| d.day).collect();
    let start = if active.contains(&today) {
        today
    } else if active.contains(&(today - 1)) {
        today - 1
    } else {
        today + 1 // forces zero
    };
    let mut d = start;
    while active.contains(&d) {
        v.streak_days += 1;
        d -= 1;
    }
    v.models = models.into_values().collect();
    v.models.sort_by_key(|m| std::cmp::Reverse(m.tokens()));
    v.days = days;
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::ModelTel;

    fn tmp() -> (tempfile::TempDir, Paths) {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().to_path_buf();
        (d, Paths { config_dir: p.join("c"), cache_dir: p.join("k"), state_dir: p.join("s") })
    }

    fn tel(models: Vec<ModelTel>) -> Telemetry {
        Telemetry { models, ..Default::default() }
    }

    fn model(name: &str, gen: f64, prompt: Option<f64>, req: Option<f64>) -> ModelTel {
        ModelTel {
            name: name.into(),
            engine: "vllm".into(),
            port: 8000,
            tokens_total: Some(gen),
            prompt_tokens_total: prompt,
            requests_total: req,
            ..Default::default()
        }
    }

    #[test]
    fn dates_round_trip_and_weekdays_match_the_calendar() {
        let day = days_from_civil(2026, 11, 6);
        assert_eq!(date_key(day), "2026-11-06");
        assert_eq!(parse_date("2026-11-06"), Some(day));
        assert_eq!(parse_date("nope"), None);
        assert_eq!(weekday(days_from_civil(1970, 1, 1)), 4, "a Thursday");
        assert_eq!(weekday(day), 5, "2026-11-06 is a Friday");
        assert_eq!(days_from_civil(2024, 2, 29), days_from_civil(2024, 3, 1) - 1);
        let (y, m, d) = civil_from_days(day);
        assert_eq!((y, m, d), (2026, 11, 6));
    }

    #[test]
    fn recorder_counts_deltas_baselines_and_restarts() {
        let (_d, paths) = tmp();
        let mut r = Recorder::new(&paths, true);
        assert!(r.enabled());
        // First sight: baseline only.
        r.observe(&tel(vec![model("qwen", 100.0, Some(10.0), Some(1.0))]));
        assert!(!r.dirty);
        // Normal deltas.
        r.observe(&tel(vec![model("qwen", 150.0, Some(20.0), Some(2.0))]));
        let store = r.store.clone();
        let (day, hour) = local_day_hour(now_secs());
        let c = store.days[&date_key(day)].hours[&hour].models["qwen"];
        assert_eq!((c.gen, c.prompt, c.requests), (50, 10, 1));
        // A restart (counter goes backwards) counts what the new process has.
        r.observe(&tel(vec![model("qwen", 7.0, Some(2.0), None)]));
        let c = r.store.days[&date_key(day)].hours[&hour].models["qwen"];
        assert_eq!((c.gen, c.prompt), (57, 12));
        assert_eq!(c.requests, 1, "an absent counter keeps its baseline");
        r.flush(true);
        let v = view(&paths, true);
        assert_eq!(v.total.gen, 57);
        assert_eq!(v.models[0].name, "qwen");
        assert!(v.has_data());
    }

    #[test]
    fn only_one_recorder_holds_the_lock() {
        let (_d, paths) = tmp();
        let a = Recorder::new(&paths, true);
        let b = Recorder::new(&paths, true);
        assert!(a.enabled());
        assert!(!b.enabled());
        drop(a);
        assert!(Recorder::new(&paths, true).enabled(), "the lock is released with the recorder");
    }

    #[test]
    fn merge_sums_hosts_and_recomputes_aggregates() {
        let now = today();
        let counts = |gen: u64, prompt: u64| Counts { gen, prompt, requests: 1 };
        let mut a = UsageView { recording: true, today: now, ..Default::default() };
        let mut hours = [0u64; 24];
        hours[10] = 110;
        a.days.push(DayView {
            day: now,
            gen: 100,
            prompt: 10,
            requests: 1,
            hours,
            models: BTreeMap::from([("qwen".to_string(), counts(100, 10))]),
        });
        let mut b = UsageView { recording: false, today: now, ..Default::default() };
        let mut hours = [0u64; 24];
        hours[10] = 55;
        b.days.push(DayView {
            day: now,
            gen: 50,
            prompt: 5,
            requests: 1,
            hours,
            models: BTreeMap::from([("qwen".to_string(), counts(50, 5)), ("glm".to_string(), counts(7, 1))]),
        });
        a.merge(&b);
        assert!(a.recording, "any recording host makes the fleet recording");
        assert_eq!(a.total.tokens(), 165);
        assert_eq!(a.today_counts.tokens(), 165);
        assert_eq!(a.active_days, 1);
        assert_eq!(a.streak_days, 1);
        assert_eq!(a.days[0].models["qwen"].tokens(), 165);
        assert_eq!(a.days[0].models["glm"].tokens(), 8);
        assert_eq!(a.all_hours[10], 165);
        assert_eq!(a.peak_hour.map(|h| h.hour), Some(10));
        assert_eq!(a.models[0].name, "qwen");
        // Merging the same host twice is additive, not idempotent: days are
        // history buckets, and a re-fetch replaces the host's view upstream.
        a.merge(&b);
        assert_eq!(a.total.tokens(), 220);
    }

    #[test]
    fn view_aggregates_models_days_hours_and_streak() {
        let (_d, paths) = tmp();
        let now = today();
        let mut store = Store::default();
        let mut add = |day: i64, hour: u8, name: &str, gen: u64, prompt: u64| {
            let d = store.days.entry(date_key(day)).or_default();
            d.hours.entry(hour).or_default().models.insert(name.into(), Counts { gen, prompt, requests: 1 });
        };
        add(now, 14, "qwen", 100, 10);
        add(now, 14, "glm", 50, 5);
        add(now, 9, "qwen", 20, 2);
        add(now - 1, 20, "qwen", 200, 20);
        add(now - 5, 3, "glm", 10, 1);
        store.save(&store_path(&paths), now).unwrap();
        let v = view(&paths, false);
        assert_eq!(v.active_days, 3);
        assert_eq!(v.total.tokens(), 418);
        assert_eq!(v.today_counts.tokens(), 187);
        assert_eq!(v.last7.tokens(), 418);
        assert_eq!(v.streak_days, 2, "today and yesterday");
        assert_eq!(v.peak_hour.map(|h| h.hour), Some(20));
        assert_eq!(v.best_day.map(|d| d.tokens), Some(220));
        assert_eq!(v.models[0].name, "qwen");
        assert_eq!(v.models[0].days, 2);
        assert_eq!(v.day(now).unwrap().models["glm"].tokens(), 55);
        assert_eq!(v.day(now).unwrap().hours[14], 165);
    }
}
