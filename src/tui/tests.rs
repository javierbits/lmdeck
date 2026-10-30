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
        family: "Other",
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
        update: None,
        weights_on: if downloaded { vec!["local".into()] } else { vec![] },
        source_repo: None,
        source_ref: None,
        weights_home: downloaded.then(|| "local".into()),
        nodes: vec![],
        placed_on: None,
        cluster_phase: None,
        cluster_error: None,
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
    a.on_key(key('4'));
    assert_eq!(a.tab, Tab::Machine);
    assert_eq!(a.on_key(code(KeyCode::Enter)), Action::None, "actions only on the deck tab");
    a.on_key(code(KeyCode::BackTab));
    assert_eq!(a.tab, Tab::Topology);
    a.on_key(code(KeyCode::BackTab));
    assert_eq!(a.tab, Tab::Hosts);
    a.on_key(code(KeyCode::Tab));
    a.on_key(code(KeyCode::Tab));
    a.on_key(code(KeyCode::Tab));
    assert_eq!(a.tab, Tab::Sources);
    a.on_key(code(KeyCode::Tab));
    assert_eq!(a.tab, Tab::Deck);
    a.on_key(key('?'));
    assert_eq!(a.mode, Mode::Help);
    a.on_key(key('q'));
    assert_eq!(a.mode, Mode::Normal);
    assert!(!a.quit, "q closes help first");
    assert_eq!(a.on_key(key('t')), Action::SaveTheme("catppuccin".into()));
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
    a.on_key(key('4'));
    let text = buffer_text(&render(&a, 140, 36));
    assert!(text.contains("system memory") && text.contains("32.0G used of 128.0G"), "{text}");
    assert!(text.contains("GB10") && text.contains("llama.cpp"), "{text}");
    a.on_key(key('5'));
    let text = buffer_text(&render(&a, 140, 36));
    assert!(text.contains("builtin") && text.contains("every recipe file parsed"), "{text}");
    a.on_key(key('?'));
    let text = buffer_text(&render(&a, 140, 36));
    assert!(text.contains("fuzzy search"), "{text}");
    // Tiny terminals must not panic.
    for (w, h) in [(20, 5), (60, 10), (1, 1)] {
        let mut b = app();
        render(&b, w, h);
        for k in ['2', '3', '4', '5'] {
            b.on_key(key(k));
            render(&b, w, h);
        }
        b.hosts = demo_hosts();
        b.topology = Some(triangle());
        render(&b, w, h);
        b.on_key(key('3'));
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

/// Two hosts with a few minutes of history; the second one is offline.
pub(super) fn demo_hosts() -> Vec<crate::hosts::HostView> {
    use crate::hosts::HostView;
    use crate::telemetry::{GpuTel, ModelTel, Sample, Telemetry};
    let tel = |name: &str, temp: f64| Telemetry {
        hostname: name.into(),
        platform: "NVIDIA GB10".into(),
        at: 1_000_000,
        unified_memory: true,
        mem_total_gb: 120.0,
        mem_used_gb: Some(60.0),
        cpu_cores: 20,
        cpu_util_pct: Some(12.0),
        system_power_w: Some(80.0),
        system_power_source: Some("spbm sys_total".into()),
        gpus: vec![GpuTel { temp_c: Some(temp), power_w: Some(55.0), util_pct: Some(90.0), ..Default::default() }],
        models: vec![ModelTel {
            name: "qwen3-4b-gguf".into(),
            engine: "llama.cpp".into(),
            port: 8080,
            pid: Some(4242),
            rss_gb: Some(6.0),
            mem_share_pct: Some(5.0),
            tokens_per_s: Some(42.5),
            metrics: Some("llama.cpp /metrics".into()),
            ..Default::default()
        }],
        sources: vec!["/proc/meminfo".into(), "nvidia-smi".into()],
        ..Default::default()
    };
    let hist: Vec<Sample> = (0..120u64)
        .map(|i| Sample {
            at: 1_000_000 - (119 - i) * 2000,
            temp_c: Some(40.0 + i as f64 * 0.2),
            power_w: Some(20.0 + i as f64),
            mem_pct: Some(50.0),
            gpu_util_pct: Some((i % 10) as f64 * 10.0),
            ..Default::default()
        })
        .collect();
    vec![
        HostView {
            name: "local".into(),
            local: true,
            online: true,
            latency_ms: Some(0),
            last_seen: Some(1_000_000),
            telemetry: Some(tel("spark1", 63.8)),
            history: hist.clone(),
            ..Default::default()
        },
        HostView {
            name: "spark2".into(),
            url: Some("http://spark2:7979".into()),
            online: false,
            error: Some("connection refused".into()),
            last_seen: Some(900_000),
            telemetry: Some(tel("spark2", 50.0)),
            history: hist,
            ..Default::default()
        },
    ]
}

#[test]
fn hosts_tab_shows_gauges_history_and_offline_hosts() {
    let mut a = app();
    a.set_hosts(demo_hosts());
    a.on_key(key('2'));
    assert_eq!(a.tab, Tab::Hosts);
    let text = buffer_text(&render(&a, 160, 42));
    for want in [
        "Hosts",
        "1/2 online",
        "local",
        "spark2",
        "64°",
        "▲6",
        "60/120G",
        "memory (unified",
        "gpu 0 util",
        "system power",
        "42.5 tok/s",
        "History",
        "spbm sys_total",
        "host",
    ] {
        assert!(text.contains(want), "missing {want:?} in\n{text}");
    }
    assert!(text.contains('▁') && text.contains('█'), "sparklines\n{text}");
    assert!(!text.contains("DEMO"), "real data is not labelled demo");
    a.on_key(key('j'));
    assert_eq!(a.host_cursor, 1);
    assert_eq!(a.cursor, 0, "recipe cursor untouched");
    let text = buffer_text(&render(&a, 160, 42));
    assert!(text.contains("offline: connection refused"), "{text}");
    a.on_key(key('j'));
    assert_eq!(a.host_cursor, 1, "clamped");
    // A refresh keeps the selected host.
    let mut h = demo_hosts();
    h.reverse();
    a.set_hosts(h);
    assert_eq!(a.selected_host().unwrap().name, "spark2");

    // Demo telemetry is always labelled.
    let mut d = demo_hosts();
    d[0].telemetry.as_mut().unwrap().demo = true;
    a.set_hosts(d);
    assert!(buffer_text(&render(&a, 160, 42)).contains("DEMO DATA"));
}

#[test]
fn deck_details_show_live_process_telemetry() {
    let mut a = app();
    a.set_hosts(demo_hosts());
    let text = buffer_text(&render(&a, 160, 42));
    assert!(text.contains("rss 6.0G (5% of RAM)") && text.contains("42.5 tok/s (llama.cpp /metrics)"), "{text}");
    assert!(text.contains("64°C 55W") || text.contains("64°C 80W"), "header shows live temp/power\n{text}");
}

#[test]
fn sparkline_buckets_and_scales() {
    use crate::telemetry::Metric;
    use crate::tui::hostsview::{bucket, change_span, scale};
    assert_eq!(bucket(&[Some(1.0)], 3), vec![None, None, Some(1.0)]);
    assert_eq!(bucket(&[Some(1.0), Some(5.0), None, Some(2.0)], 2), vec![Some(5.0), Some(2.0)]);
    assert_eq!(bucket(&[None, None], 1), vec![None]);
    assert!(bucket(&[Some(1.0)], 0).is_empty());
    assert_eq!(scale(Metric::Mem, &[Some(250.0)]), (0.0, 100.0));
    assert!((scale(Metric::Power, &[Some(100.0)]).1 - 115.0).abs() < 1e-9);
    let t = crate::tui::theme::omp();
    assert_eq!(change_span(Some(2.4), true, t).content, "▲2");
    assert_eq!(change_span(Some(-1.0), true, t).content, "▼1");
    assert_eq!(change_span(Some(0.1), true, t).content, "·");
    assert_eq!(change_span(None, true, t).content, "");
}

#[test]
fn theme_picker_previews_saves_and_reverts() {
    let mut a = app();
    let n = crate::tui::theme::themes().len();
    a.on_key(key('T'));
    assert_eq!(a.mode, Mode::Themes);
    assert_eq!(a.theme_cursor, 0);
    // Moving previews live, without saving.
    assert_eq!(a.on_key(code(KeyCode::Down)), Action::None);
    assert_eq!(a.on_key(key('j')), Action::None);
    assert_eq!(a.theme().name, "catppuccin-latte");
    assert!(a.theme().light);
    let frame = buffer_text(&render(&a, 150, 40));
    assert!(frame.contains("Theme") && frame.contains("Rosé Pine") && frame.contains("esc revert"), "{frame}");
    // Esc goes back to what was there before.
    a.on_key(code(KeyCode::Esc));
    assert_eq!((a.mode, a.theme().name), (Mode::Normal, "omp"));
    // Enter keeps the highlighted theme and asks the runtime to save it.
    a.on_key(key('T'));
    a.on_key(key('G'));
    assert_eq!(a.theme_cursor, n - 1);
    a.on_key(code(KeyCode::Down)); // stays on the last one
    assert_eq!(a.on_key(code(KeyCode::Enter)), Action::SaveTheme("white".into()));
    assert_eq!((a.mode, a.theme().name), (Mode::Normal, "white"));
    // The picker scrolls to keep the cursor visible in a short terminal.
    a.on_key(key('T'));
    let frame = buffer_text(&render(&a, 100, 16));
    assert!(frame.contains("▸ White"), "{frame}");
}

#[test]
fn every_theme_renders() {
    let mut a = app();
    for i in 0..crate::tui::theme::themes().len() {
        a.theme = i;
        for tab in Tab::ALL {
            a.tab = tab;
            let _ = render(&a, 120, 36);
        }
    }
}

fn upd(state: crate::updates::State) -> crate::updates::UpdateView {
    crate::updates::UpdateView {
        state,
        repo: "https://github.com/x/wrapped".into(),
        git_ref: Some("main".into()),
        pinned: state == crate::updates::State::PinnedNewer,
        follows: Some("main".into()),
        local: Some("1111111aaaa".into()),
        remote: Some("2222222bbbb".into()),
        behind: Some(3),
        checked_at: Some(0),
        error: None,
    }
}

#[test]
fn wrapped_recipes_show_an_update_badge_and_u_takes_it() {
    use crate::updates::State as U;
    let mut s = demo();
    s.recipes[0].update = Some(upd(U::Behind));
    s.recipes[1].update = Some(upd(U::Current));
    s.recipes[2].update = Some(upd(U::PinnedNewer));
    let mut a = App::default();
    a.set_snapshot(s);
    let text = buffer_text(&render(&a, 160, 42));
    assert!(text.contains("qwen3-4b-gguf ⬆ update"), "{text}");
    assert!(text.contains("glm-big-exl3 ⬆ newer"), "{text}");
    assert!(!text.contains("gpt-oss-20b-vllm ⬆"), "{text}");
    assert!(text.contains("update available (main): 1111111 → 2222222 (3 commits)"), "{text}");
    // The badge uses the theme's warn colour.
    let buf = render(&a, 160, 42);
    let warn = a.theme().warn;
    let found = (0..buf.area.height)
        .any(|y| (0..buf.area.width).any(|x| buf[(x, y)].symbol() == "⬆" && buf[(x, y)].fg == warn));
    assert!(found, "⬆ drawn in the warn colour");
    let sel = |a: &App| a.selected().unwrap().reference.clone();
    assert_eq!(a.on_key(key('u')), Action::Update(sel(&a)));
    a.on_key(key('j'));
    assert_eq!(a.on_key(key('u')), Action::None, "nothing to take on an up-to-date recipe");
    a.on_key(key('j'));
    assert_eq!(a.on_key(key('u')), Action::Update(sel(&a)));
    let text = buffer_text(&render(&a, 160, 42));
    assert!(text.contains("pinned; upstream (main) is newer"), "{text}");
}

/// When the list is too narrow for `name ⬆ update`, the name gets an
/// ellipsis and the badge shrinks to `⬆`; the badge is never cut.
#[test]
fn update_badge_survives_a_narrow_list() {
    use crate::updates::State as U;
    let mut s = demo();
    s.recipes[0].name = "a-very-long-wrapped-recipe-name".into();
    s.recipes[0].update = Some(upd(U::Behind));
    let mut a = App::default();
    a.set_snapshot(s);
    let narrow = buffer_text(&render(&a, 100, 30));
    assert!(narrow.contains("… ⬆ "), "{narrow}");
    assert!(!narrow.contains("⬆ upd ") && !narrow.contains("⬆ updat"), "{narrow}");
    // Wide enough: the list grows (up to 62%) to show the whole badge.
    let wide = buffer_text(&render(&a, 180, 42));
    assert!(wide.contains("a-very-long-wrapped-recipe-name ⬆ update"), "{wide}");
}

/// Three Sparks cabled in a triangle (constructed, like tests/fixtures/fabric).
pub(super) fn triangle() -> crate::topology::Topology {
    use crate::fabric::{End, Link, LinkKind};
    use crate::topology::{TopoHost, TopoLink, Topology};
    let host = |key: &str, name: &str, local: bool| TopoHost {
        key: key.into(),
        name: format!("{name}.lab"),
        alias: Some(name.into()),
        model: Some("DGX Spark".into()),
        device_kind: "spark".into(),
        local,
        online: true,
        gpus: vec![crate::topology::TopoGpu {
            index: 0,
            name: "NVIDIA GB10".into(),
            mem_total_gb: None,
            temp_c: Some(52.0),
            util_pct: Some(40.0),
            power_w: Some(80.0),
            ..Default::default()
        }],
        mem_total_gb: Some(120.0),
        running: if local { vec!["qwen3-4b-gguf".into()] } else { vec![] },
        weights: vec!["Qwen/Qwen3-4B".into()],
        mgmt_kind: if local { "local".into() } else { "lan".into() },
        mgmt_addr: if local { None } else { Some(format!("192.168.1.{}", name.len())) },
        ..Default::default()
    };
    let end = |h: &str, ip: &str| End {
        host: h.into(),
        ifname: Some("enp1s0f1np1".into()),
        ip: ip.into(),
        ..Default::default()
    };
    let link = |a: (&str, &str), b: (&str, &str), active: Vec<String>, ok: Option<bool>| TopoLink {
        link: Link {
            a: end(a.0, a.1),
            b: end(b.0, b.1),
            kind: LinkKind::Cx7,
            subnet: Some("10.0.0.0/30".into()),
            speed_mbps: Some(200_000),
            manual: false,
            verified: ok,
            rtt_ms: ok.map(|_| 0.12),
        },
        active,
    };
    Topology {
        at: 1,
        demo: false,
        controller: "local".into(),
        hosts: vec![host("local", "spark1", true), host("spark2", "spark2", false), host("spark3", "spark3", false)],
        switches: vec![],
        links: vec![
            link(("local", "10.0.0.1"), ("spark2", "10.0.0.2"), vec!["nfs weights: qwen".into()], Some(true)),
            link(("spark2", "10.0.1.1"), ("spark3", "10.0.1.2"), vec![], Some(false)),
            link(("local", "10.0.2.1"), ("spark3", "10.0.2.2"), vec![], None),
        ],
        segments: vec![],
        notes: vec![],
        ..Default::default()
    }
}

#[test]
fn topology_tab_draws_boxes_cables_and_the_link_table() {
    let mut a = app();
    a.set_hosts(demo_hosts());
    a.on_key(key('3'));
    assert_eq!(a.tab, Tab::Topology);
    let text = buffer_text(&render(&a, 140, 40));
    assert!(text.contains("detecting fabric"), "{text}");
    a.topology = Some(triangle());
    let text = buffer_text(&render(&a, 140, 40));
    for want in
        ["spark1.lab", "spark2.lab", "spark3.lab", "NVIDIA GB10", "DGX Spark", "nfs weights: qwen", "200G", "cx7", "━"]
    {
        assert!(text.contains(want), "missing {want:?} in\n{text}");
    }
    assert!(text.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)), "expected braille icon dots in\n{text}");
    let text = buffer_text(&render(&a, 60, 30));
    assert!(text.contains("Topology"), "{text}");
}

fn rows() -> Vec<crate::tui::RunOnRow> {
    use crate::tui::RunOnRow;
    vec![
        RunOnRow {
            host: "local".into(),
            label: "spark1 (this machine)".into(),
            mode: "error".into(),
            summary: String::new(),
            notes: vec!["needs ~200 GB, spark1 has 120 GB".into()],
            ok: false,
        },
        RunOnRow {
            host: "spark2".into(),
            label: "spark2".into(),
            mode: "nfs".into(),
            summary: "weights on spark1 → NFS ro · cx7 · load ≈12 s · 60/120 GB".into(),
            notes: vec![],
            ok: true,
        },
    ]
}

#[test]
fn run_on_picker_plans_then_starts_on_the_chosen_host() {
    let mut a = app();
    let r = a.selected().unwrap().clone();
    let Action::PlanRunOn(reference) = a.on_key(key('o')) else { panic!("o plans") };
    assert_eq!(reference, r.reference);
    assert_eq!(a.mode, Mode::RunOn);
    assert!(buffer_text(&render(&a, 140, 36)).contains("planning on every host"));
    a.set_plans("someone-else", rows());
    assert!(a.run_on.as_ref().unwrap().rows.is_empty(), "plans for another recipe are ignored");
    a.set_plans(&reference, rows());
    assert_eq!(a.run_on.as_ref().unwrap().cursor, 1, "cursor starts on the first host that works");
    let text = buffer_text(&render(&a, 140, 36));
    for want in ["Run ", "spark2", "nfs ro", "load ≈12 s", "can't", "needs ~200 GB", "weights never move"] {
        assert!(text.contains(want), "missing {want:?} in\n{text}");
    }
    a.on_key(code(KeyCode::Up));
    assert_eq!(a.on_key(code(KeyCode::Enter)), Action::None, "a host that can't run it is refused");
    assert_eq!(a.mode, Mode::RunOn);
    a.on_key(code(KeyCode::Down));
    let act = a.on_key(code(KeyCode::Enter));
    assert_eq!(act, Action::StartOn { reference: r.reference.clone(), host: "spark2".into(), only: false });
    assert_eq!(a.mode, Mode::Normal);
    assert!(a.run_on.is_none() && a.busy.contains(&r.name));
    // s = switch there; esc cancels.
    a.on_key(key('o'));
    a.set_plans(&r.reference, rows());
    assert_eq!(
        a.on_key(key('s')),
        Action::StartOn { reference: r.reference.clone(), host: "spark2".into(), only: true }
    );
    a.on_key(key('o'));
    a.on_key(code(KeyCode::Esc));
    assert_eq!(a.mode, Mode::Normal);
}

#[test]
fn run_on_refuses_multi_node_recipes() {
    let mut a = app();
    let mut s = demo();
    s.recipes[0].nodes = vec![crate::cluster::NodeView {
        name: "head".into(),
        role: "head".into(),
        host: "local".into(),
        served_as: "x.head".into(),
        status: "stopped".into(),
    }];
    a.set_snapshot(s);
    a.cursor = a.visible().iter().position(|r| !r.nodes.is_empty()).unwrap();
    assert_eq!(a.on_key(key('o')), Action::None);
    assert_eq!(a.mode, Mode::Normal);
}

#[test]
fn hosts_tab_controls_reach_the_selected_host() {
    let mut a = app();
    a.set_hosts(demo_hosts());
    let r = a.selected().unwrap().clone();
    a.on_key(key('2'));
    a.on_key(code(KeyCode::Down));
    assert_eq!(a.selected_host_key().as_deref(), Some("spark2"));
    assert_eq!(
        a.on_key(code(KeyCode::Enter)),
        Action::StartOn { reference: r.reference.clone(), host: "spark2".into(), only: false }
    );
    assert_eq!(
        a.on_key(key('s')),
        Action::StartOn { reference: r.reference.clone(), host: "spark2".into(), only: true }
    );
    assert_eq!(a.on_key(key('x')), Action::StopOn { host: "spark2".into(), name: "qwen3-4b-gguf".into() });
    let text = buffer_text(&render(&a, 150, 44));
    assert!(text.contains("x stop · l logs") && text.contains(&format!("run {} here", r.name)), "{text}");
    assert_eq!(a.on_key(key('l')), Action::LogsOn { host: "spark2".into(), name: "qwen3-4b-gguf".into() });
    assert_eq!(a.remote_log, Some(("spark2".into(), "qwen3-4b-gguf".into())));
    assert_eq!(a.tab, Tab::Deck);
    assert!(a.logs_max);
    a.on_key(code(KeyCode::Esc));
    assert!(a.remote_log.is_none() && !a.logs_max);
    // This machine is `local` to the actions.
    a.on_key(key('2'));
    a.on_key(code(KeyCode::Up));
    assert_eq!(a.on_key(key('x')), Action::StopOn { host: "local".into(), name: "qwen3-4b-gguf".into() });
}

#[test]
fn hosts_tab_wont_place_a_multi_node_recipe_on_one_host() {
    let mut a = app();
    let mut s = demo();
    s.recipes[0].nodes = vec![crate::cluster::NodeView {
        name: "head".into(),
        role: "head".into(),
        host: "local".into(),
        served_as: "x.head".into(),
        status: "stopped".into(),
    }];
    let name = s.recipes[0].name.clone();
    a.set_snapshot(s);
    a.set_hosts(demo_hosts());
    a.cursor = a.visible().iter().position(|r| r.name == name).unwrap();
    a.on_key(key('2'));
    a.on_key(code(KeyCode::Down));
    assert_eq!(a.on_key(code(KeyCode::Enter)), Action::None);
    assert!(a.busy.is_empty());
}

#[test]
fn list_marks_weights_that_live_on_another_host() {
    let mut a = app();
    let mut s = demo();
    a.grouped = false; // the badge is what this test checks, not the grouping
    let r = &mut s.recipes[0];
    r.downloaded = false;
    r.weights_on = vec!["spark2".into()];
    let name = r.name.clone();
    a.set_snapshot(s);
    let text = buffer_text(&render(&a, 170, 50));
    let row = text.lines().find(|l| l.contains(&name)).unwrap();
    assert!(row.contains("⛁spark2") && row.matches('⛁').count() == 2, "{row}");
}

#[test]
fn html_frames_pin_fallback_glyphs_to_their_cells() {
    let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 6, 1));
    buf.set_string(0, 0, "a⛁│", ratatui::style::Style::default());
    let html = super::buffer_html(&buf, "t");
    assert!(html.contains("a<i style=\"width:1ch\">⛁</i>│"), "{html}");
}

#[test]
fn details_and_list_show_weights_home_nodes_and_placement() {
    let mut a = app();
    let mut s = demo();
    let r = &mut s.recipes[0];
    r.weights_on = vec!["spark1".into(), "spark2".into()];
    r.weights_home = Some("spark1".into());
    r.placed_on = Some("spark2".into());
    r.nodes = vec![
        crate::cluster::NodeView {
            name: "head".into(),
            role: "head".into(),
            host: "spark1".into(),
            served_as: "x.head".into(),
            status: "loaded".into(),
        },
        crate::cluster::NodeView {
            name: "w1".into(),
            role: "worker".into(),
            host: "spark2".into(),
            served_as: "x.w1".into(),
            status: "offline".into(),
        },
    ];
    r.cluster_phase = Some("failed".into());
    r.cluster_error = Some("w1 never came up".into());
    let name = r.name.clone();
    a.set_snapshot(s);
    a.cursor = a.visible().iter().position(|r| r.name == name).unwrap();
    let text = buffer_text(&render(&a, 170, 50));
    for want in [
        "⛁ spark1, spark2",
        "home spark1",
        "on spark2 (weights read over NFS",
        "2 nodes, started in order · failed",
        "worker",
        "● offline",
        "✗ w1 never came up",
        "×2",
        "→spark2",
    ] {
        assert!(text.contains(want), "missing {want:?} in\n{text}");
    }
}
