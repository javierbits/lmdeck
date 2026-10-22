use super::*;
use crate::compat::Check;
use crate::deck::RecipeView;
use crate::engine::{Inventory, ToolStatus};
use crate::hardware::{Gpu, Hardware};
use crate::runner::Status;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;

fn recipe(name: &str, engine: &str, status: Status, downloaded: bool, fit: Level, listed: bool) -> RecipeView {
    RecipeView {
        index: 0,
        name: name.into(),
        reference: format!("{}/{name}", if listed { "deck" } else { "community" }),
        origin: if listed { "deck".into() } else { "community".into() },
        display_name: name.replace('-', " "),
        description: Some(format!("{name} test recipe")),
        tags: vec!["chat".into()],
        engine: engine.into(),
        quant: "Q4_K_M".into(),
        model: Some(format!("org/{name}")),
        revision: Some("0123456789abcdef".into()),
        port: 8080,
        ctx: Some(8192),
        endpoint: "http://localhost:8080/v1".into(),
        listed,
        status,
        pid: None,
        started_at: None,
        downloaded,
        cached: downloaded,
        size_bytes: if downloaded { 4_000_000_000 } else { 0 },
        size_gb: Some(4.0),
        need_gb: Some(4.8),
        fit,
        checks: vec![Check { level: fit, what: "memory".into(), detail: "needs ~4.8 GB of 64 GB".into() }],
        command: Some(format!("llama-server -m {name}.gguf --port 8080")),
        plan_error: None,
        pulling: false,
    }
}

pub(super) fn demo() -> Snapshot {
    let mut recipes = vec![
        recipe("qwen3-4b-gguf", "llama.cpp", Status::Loaded, true, Level::Ok, true),
        recipe("gpt-oss-20b-vllm", "vllm", Status::Stopped, false, Level::Warn, true),
        recipe("glm-big-exl3", "tabbyapi", Status::Stopped, false, Level::Fail, true),
        recipe("qwen3.5-2b-tensorfold", "tensorfold", Status::Stopped, true, Level::Ok, false),
    ];
    for (i, r) in recipes.iter_mut().enumerate() {
        r.index = if r.listed { i + 1 } else { 0 };
    }
    let mut engines = BTreeMap::new();
    engines.insert(
        "llama.cpp".into(),
        ToolStatus { name: "llama.cpp".into(), available: true, detail: "/usr/bin/llama-server".into() },
    );
    engines
        .insert("vllm".into(), ToolStatus { name: "vllm".into(), available: false, detail: "pip install vllm".into() });
    Snapshot {
        version: "0.1.0".into(),
        generated_at: 0,
        hardware: Hardware {
            os: "linux".into(),
            arch: "aarch64".into(),
            hostname: "spark".into(),
            cpu: "Cortex-X925".into(),
            cores: 20,
            total_memory_gb: 128.0,
            available_memory_gb: Some(96.0),
            gpus: vec![Gpu {
                vendor: "nvidia".into(),
                name: "GB10".into(),
                memory_gb: None,
                compute_capability: Some("12.1".into()),
                driver: None,
            }],
            unified_memory: true,
            platform: "NVIDIA GB10 (DGX Spark class)".into(),
        },
        inventory: Inventory { engines, tools: BTreeMap::new() },
        recipes,
        sources: vec![],
        recipe_errors: vec![],
    }
}

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn code(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn app() -> App {
    let mut a = App::default();
    a.set_snapshot(demo());
    a
}

#[test]
fn fuzzy_prefers_tight_matches() {
    assert!(app::fuzzy_score("q4b", "qwen3-4b-gguf").is_some());
    assert!(app::fuzzy_score("zzz", "qwen3-4b-gguf").is_none());
    let tight = app::fuzzy_score("vllm", "gpt-oss-20b-vllm vllm").unwrap();
    let loose = app::fuzzy_score("vllm", "v l l m").unwrap();
    assert!(tight > loose);
    // every word must match
    assert!(app::fuzzy_score("qwen tensorfold", "qwen3.5-2b-tensorfold").is_some());
    assert!(app::fuzzy_score("qwen vllm", "qwen3-4b-gguf llama.cpp").is_none());
}

#[test]
fn deck_shows_listed_and_all_toggles() {
    let mut a = app();
    assert_eq!(a.visible().len(), 3);
    a.on_key(key('a'));
    assert_eq!(a.visible().len(), 4);
    a.on_key(key('f'));
    let names: Vec<_> = a.visible().iter().map(|r| r.name.clone()).collect();
    assert!(!names.contains(&"glm-big-exl3".to_string()), "{names:?}");
    assert_eq!(names.len(), 3);
}

#[test]
fn navigation_clamps_and_survives_refresh() {
    let mut a = app();
    a.on_key(code(KeyCode::Up));
    assert_eq!(a.cursor, 0);
    a.on_key(key('j'));
    a.on_key(key('j'));
    a.on_key(key('j'));
    assert_eq!(a.cursor, 2);
    assert_eq!(a.selected().unwrap().name, "glm-big-exl3");
    // A refresh that reorders recipes keeps the same one selected.
    let mut s = demo();
    s.recipes.reverse();
    a.set_snapshot(s);
    assert_eq!(a.selected().unwrap().name, "glm-big-exl3");
    a.on_key(key('g'));
    assert_eq!(a.cursor, 0);
}

#[test]
fn enter_toggles_by_status_and_marks_busy() {
    let mut a = app();
    assert_eq!(a.on_key(code(KeyCode::Enter)), Action::Stop("deck/qwen3-4b-gguf".into()));
    assert!(a.busy.contains(&"qwen3-4b-gguf".to_string()));
    a.on_key(key('j'));
    assert_eq!(a.on_key(code(KeyCode::Enter)), Action::Start("deck/gpt-oss-20b-vllm".into()));
    assert_eq!(a.on_key(key('s')), Action::Switch("deck/gpt-oss-20b-vllm".into()));
    assert_eq!(a.on_key(key('p')), Action::Pull("deck/gpt-oss-20b-vllm".into()));
    assert_eq!(a.on_key(key('x')), Action::None, "x on a stopped recipe does nothing");
    // A changed status clears the busy marker.
    let mut s = demo();
    s.recipes[0].status = Status::Stopped;
    a.set_snapshot(s);
    assert!(!a.busy.contains(&"qwen3-4b-gguf".to_string()));
}

#[test]
fn search_mode_edits_query() {
    let mut a = app();
    a.on_key(key('/'));
    assert_eq!(a.mode, Mode::Search);
    for c in "vllm".chars() {
        a.on_key(key(c));
    }
    // typing 'q' in search does not quit
    assert!(!a.quit);
    assert_eq!(a.visible().len(), 1);
    assert_eq!(a.selected().unwrap().name, "gpt-oss-20b-vllm");
    a.on_key(code(KeyCode::Enter));
    assert_eq!(a.mode, Mode::Normal);
    assert_eq!(a.query, "vllm");
    a.on_key(code(KeyCode::Esc));
    assert!(a.query.is_empty());
    assert!(!a.quit);
    a.on_key(key('/'));
    a.on_key(key('x'));
    a.on_key(code(KeyCode::Backspace));
    a.on_key(code(KeyCode::Esc));
    assert_eq!(a.visible().len(), 3);
}

#[test]
fn tabs_help_theme_and_quit() {
    let mut a = app();
    a.on_key(key('2'));
    assert_eq!(a.tab, Tab::Machine);
    assert_eq!(a.on_key(code(KeyCode::Enter)), Action::None, "actions only on the deck tab");
    a.on_key(code(KeyCode::Tab));
    assert_eq!(a.tab, Tab::Sources);
    a.on_key(code(KeyCode::Tab));
    assert_eq!(a.tab, Tab::Deck);
    a.on_key(key('?'));
    assert_eq!(a.mode, Mode::Help);
    a.on_key(key('q'));
    assert_eq!(a.mode, Mode::Normal);
    assert!(!a.quit, "q closes help first");
    a.on_key(key('t'));
    assert_eq!(a.theme().name, "catppuccin");
    assert_eq!(a.on_key(key('r')), Action::Refresh);
    assert_eq!(a.on_key(key('q')), Action::Quit);
    assert!(a.quit);
    let mut b = app();
    b.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(b.quit);
}

#[test]
fn logs_pane_scrolls_when_maximized() {
    let mut a = app();
    a.logs = (0..50).map(|i| format!("line {i}")).collect();
    a.on_key(key('l'));
    assert!(a.logs_max);
    a.on_key(key('k'));
    a.on_key(key('k'));
    assert_eq!(a.log_scroll, 2);
    a.on_key(key('j'));
    assert_eq!(a.log_scroll, 1);
    assert_eq!(a.cursor, 0, "list cursor untouched");
    a.on_key(code(KeyCode::Esc));
    assert!(!a.logs_max && !a.quit);
}

#[test]
fn renders_every_tab_and_overlay() {
    let mut a = app();
    a.logs = vec!["server listening on 127.0.0.1:8080".into(), "error: boom".into()];
    let text = buffer_text(&render(&a, 140, 36));
    for want in
        ["lmdeck", "Deck", "qwen3-4b-gguf", "running", "fits", "no fit", "Logs qwen3-4b-gguf", "listening", "switch"]
    {
        assert!(text.contains(want), "missing {want:?} in\n{text}");
    }
    a.on_key(key('2'));
    let text = buffer_text(&render(&a, 140, 36));
    assert!(text.contains("system memory") && text.contains("32.0G used of 128.0G"), "{text}");
    assert!(text.contains("GB10") && text.contains("llama.cpp"), "{text}");
    a.on_key(key('3'));
    let text = buffer_text(&render(&a, 140, 36));
    assert!(text.contains("builtin") && text.contains("every recipe file parsed"), "{text}");
    a.on_key(key('?'));
    let text = buffer_text(&render(&a, 140, 36));
    assert!(text.contains("fuzzy search"), "{text}");
    // Tiny terminals must not panic.
    for (w, h) in [(20, 5), (60, 10), (1, 1)] {
        let mut b = app();
        render(&b, w, h);
        b.on_key(key('2'));
        render(&b, w, h);
    }
    let html = buffer_html(&render(&a, 80, 20), "x");
    assert!(html.starts_with("<!doctype html>") && html.contains("background:#"));
}

#[test]
fn loading_screen_before_first_snapshot() {
    let a = App::default();
    assert!(buffer_text(&render(&a, 80, 10)).contains("probing hardware"));
}

#[test]
fn first_snapshot_selects_the_running_recipe() {
    let mut s = demo();
    s.recipes.swap(0, 2); // running recipe is no longer first
    let mut a = App::default();
    a.set_snapshot(s.clone());
    assert_eq!(a.selected().unwrap().name, "qwen3-4b-gguf");
    // Later refreshes keep whatever the user picked.
    a.on_key(key('g'));
    a.set_snapshot(s);
    assert_eq!(a.cursor, 0);
}
