//! The hub's two pure rules and its record. A blip: a host whose snapshot
//! can't be fetched keeps the lanes it last answered with through
//! `KEEP_MISSES` failed looks in a row and loses them on the next, a host
//! that answers has exactly what it said, and a host no longer looked at is
//! forgotten. A push: a host's harnesses get the pushed lanes, then its own
//! ones the hub didn't list. The record: what the hub knows of every host
//! survives a save and a load, and a push another one overlapped claims no
//! host.

use lmdeck::harness::Lane;
use lmdeck::hub::{Exclusive, LastPush, Memory, Pushed, KEEP_MISSES};
use lmdeck::paths::Paths;
use proptest::prelude::*;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// The hosts the hub looks at.
const HOSTS: [&str; 3] = ["spark2", "spark3", "gigabyte"];

/// Returns a lane for model `name` served on `host`.
fn lane(name: &str, host: &str) -> Lane {
    Lane {
        name: name.into(),
        display: name.into(),
        host: host.into(),
        port: 8000,
        ctx: 65_536,
        vision: false,
        remote: true,
        chat: true,
    }
}

/// What one host did at one look.
#[derive(Debug, Clone)]
enum Look {
    /// It wasn't looked at (gone from hosts.toml).
    Gone,
    /// Its snapshot couldn't be fetched.
    Failed,
    /// It answered, serving these models.
    Answered(Vec<String>),
}

/// Returns any one look, failures as likely as answers.
fn look() -> impl Strategy<Value = Look> {
    prop_oneof![
        1 => Just(Look::Gone),
        3 => Just(Look::Failed),
        3 => prop::collection::vec("[a-z]{1,6}", 0..3).prop_map(Look::Answered),
    ]
}

/// The reference model of one host: the models of its last answer and the
/// failed looks since, both forgotten when it isn't looked at.
#[derive(Debug, Clone, Default)]
struct Model {
    /// The models it last answered with.
    last: Option<Vec<String>>,
    /// Failed looks in a row since then.
    misses: u32,
}

proptest! {
    /// After every look, each host the hub looked at is remembered with
    /// exactly the lanes and miss count the reference model gives, a host it
    /// didn't look at is forgotten, and the lanes counted are the remembered
    /// ones, host by host.
    #[test]
    fn a_host_keeps_its_lanes_through_two_failed_looks(
        looks in prop::collection::vec(prop::collection::vec(look(), HOSTS.len()), 1..40)
    ) {
        let mut memory = Memory::default();
        let mut models = vec![Model::default(); HOSTS.len()];
        for round in &looks {
            let polled: Vec<(String, Option<Vec<Lane>>)> = HOSTS
                .iter()
                .zip(round)
                .filter_map(|(h, l)| match l {
                    Look::Gone => None,
                    Look::Failed => Some((h.to_string(), None)),
                    Look::Answered(names) => Some((h.to_string(), Some(names.iter().map(|n| lane(n, h)).collect()))),
                })
                .collect();
            memory = memory.after(polled);
            for ((h, l), m) in HOSTS.iter().zip(round).zip(models.iter_mut()) {
                match l {
                    Look::Gone => *m = Model::default(),
                    Look::Failed => m.misses += 1,
                    Look::Answered(names) => *m = Model { last: Some(names.clone()), misses: 0 },
                }
                let Some(seen) = memory.get(h) else {
                    prop_assert!(matches!(l, Look::Gone), "{} was looked at but isn't remembered", h);
                    continue;
                };
                prop_assert!(!matches!(l, Look::Gone), "{} wasn't looked at but is remembered", h);
                prop_assert_eq!(seen.misses, m.misses);
                let kept = if m.misses <= KEEP_MISSES { m.last.clone().unwrap_or_default() } else { vec![] };
                let want: Vec<Lane> = kept.iter().map(|n| lane(n, h)).collect();
                prop_assert_eq!(&seen.lanes, &want, "{} after {} miss(es)", h, m.misses);
            }
            let counted: Vec<Lane> = HOSTS.iter().filter_map(|h| memory.get(h)).flat_map(|s| s.lanes.clone()).collect();
            prop_assert_eq!(memory.lanes(), counted);
        }
    }
}

/// The pushed lanes come first and win by name (they carry the address the
/// hub's host list gives); an own lane the hub didn't list joins them; a
/// model pushed once stays lmdeck's to rewrite or remove in hermes.
#[test]
fn pushed_lanes_come_first_and_win_by_name() {
    let pushed = Pushed {
        from: "spark".into(),
        at: 0,
        lanes: vec![lane("glm", "spark2.local"), lane("qwen", "gigabyte.local")],
        names: BTreeSet::from(["glm".to_string(), "qwen".to_string(), "gone".to_string()]),
    };
    let own = (vec![lane("qwen", "10.0.0.9"), lane("mine", "10.0.0.9")], BTreeSet::from(["mine".to_string()]));
    let (lanes, managed) = pushed.merge(own);
    let got: Vec<(&str, &str)> = lanes.iter().map(|l| (l.name.as_str(), l.host.as_str())).collect();
    assert_eq!(got, [("glm", "spark2.local"), ("qwen", "gigabyte.local"), ("mine", "10.0.0.9")]);
    assert_eq!(managed, BTreeSet::from(["glm", "qwen", "gone", "mine"].map(String::from)));
}

/// Nothing pushed yet (an empty push) leaves a host's own lanes as they are.
#[test]
fn an_empty_push_leaves_the_own_lanes() {
    let own = (vec![lane("mine", "10.0.0.9")], BTreeSet::from(["mine".to_string()]));
    let (lanes, managed) = Pushed::default().merge(own.clone());
    assert_eq!((lanes, managed), own);
}

/// Returns the directories of an lmdeck home under `root`.
fn paths_in(root: &Path) -> Paths {
    let home = root.join("home").display().to_string();
    Paths::resolve(|k| (k == "LMDECK_HOME").then(|| home.clone()), root.to_path_buf())
}

/// The record keeps every host's lanes and failed looks in a row, in
/// hosts.toml order, through a save and a load: what the next push, or a
/// restarted serve's first look, starts from.
#[test]
fn the_record_survives_a_round_trip_with_miss_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let answered = |h: &str, names: &[&str]| (h.to_string(), Some(names.iter().map(|n| lane(n, h)).collect()));
    let failed = |h: &str| (h.to_string(), None);
    let hosts = Memory::default()
        .after(vec![answered("spark2", &[]), answered("spark3", &["qwen"]), failed("gigabyte")])
        .after(vec![answered("spark2", &["glm"]), failed("spark3"), failed("gigabyte")])
        .after(vec![failed("spark2"), failed("spark3"), failed("gigabyte")])
        .after(vec![failed("spark2"), failed("spark3"), answered("gigabyte", &["oss"])]);
    let record = LastPush { lanes: hosts.lanes(), took: BTreeSet::from(["gigabyte".to_string()]), hosts, round: 4 };
    record.save(&paths, &Exclusive::take(&paths)).unwrap();
    let back = LastPush::load(&paths).expect("the saved record reads back");
    assert_eq!(back, record);
    let seen: Vec<(&str, u32, usize)> =
        back.hosts.hosts.iter().map(|(h, s)| (h.as_str(), s.misses, s.lanes.len())).collect();
    assert_eq!(seen, [("spark2", 2, 1), ("spark3", 3, 0), ("gigabyte", 0, 1)]);
}

/// A record written before the hub remembered hosts (no `hosts`, no
/// `round`) still reads: its lanes and claims stay, and the memory starts
/// empty.
#[test]
fn a_record_from_before_the_memory_still_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    fs::create_dir_all(&paths.state_dir).unwrap();
    let old = serde_json::json!({"lanes": [lane("glm", "spark2")], "took": ["spark2"]});
    fs::write(paths.state_dir.join("harness-hub.json"), old.to_string()).unwrap();
    let want = LastPush {
        lanes: vec![lane("glm", "spark2")],
        took: BTreeSet::from(["spark2".to_string()]),
        ..Default::default()
    };
    assert_eq!(LastPush::load(&paths), Some(want));
}

/// A push claims the hosts that took it while no other push overlapped it.
/// Once one did, either may land last on a host, so the first of the two to
/// claim voids every claim and the round, whichever it is: the next look
/// pushes to every host again.
#[test]
fn an_overlapped_push_claims_no_host() {
    let mut alone = LastPush { round: 7, ..Default::default() };
    alone.claim(7, &["spark2".into()]);
    assert_eq!(alone.took, BTreeSet::from(["spark2".to_string()]));
    // Pushes 6 and 7 overlapped: the record is at round 7 once both started.
    for order in [[6, 7], [7, 6]] {
        let mut record = LastPush { round: 7, ..Default::default() };
        for round in order {
            record.claim(round, &["spark2".into()]);
        }
        assert!(record.took.is_empty(), "claims {order:?} left {:?}", record.took);
    }
}
