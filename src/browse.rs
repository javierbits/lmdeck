//! Browse: the recipe index joined with what this machine has, for
//! `lmdeck browse`.

use serde::Serialize;

use crate::compat::Level;
use crate::deck::Snapshot;
use crate::index::{Entry, Index};

/// One index entry as Browse shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    #[serde(flatten)]
    pub entry: Entry,
    /// `deck` (in your deck), `builtin` (ships with lmdeck, not in the deck) or `no`.
    pub installed: &'static str,
    /// The deck/built-in recipe it matched.
    pub installed_as: Option<String>,
    /// `⬆ …` text when there is something newer, with what to run.
    pub update: Option<String>,
    pub fit: Level,
    pub fit_reason: String,
    pub readme: String,
    pub install: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    #[default]
    Name,
    Sparks,
    Speed,
}

impl Sort {
    pub const ALL: [Sort; 3] = [Sort::Name, Sort::Sparks, Sort::Speed];

    pub fn parse(s: &str) -> Option<Sort> {
        match s.trim().to_lowercase().as_str() {
            "name" => Some(Sort::Name),
            "sparks" | "spark" | "nodes" => Some(Sort::Sparks),
            "speed" | "tok/s" | "toks" => Some(Sort::Speed),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::Sparks => "sparks",
            Sort::Speed => "speed",
        }
    }

    pub fn next(self) -> Sort {
        Sort::ALL[(Sort::ALL.iter().position(|s| *s == self).unwrap_or(0) + 1) % Sort::ALL.len()]
    }
}

fn norm_repo(r: &str) -> String {
    r.trim().trim_end_matches('/').trim_end_matches(".git").to_lowercase()
}

/// Joins the index with the snapshot. `sparks` counts the DGX Sparks online
/// ([`crate::hosts::fleet`]); Macs and other hosts don't count.
pub fn rows(index: &Index, snap: Option<&Snapshot>, sparks: u32) -> Vec<Row> {
    index.entries.iter().map(|e| row(e, snap, sparks)).collect()
}

fn row(e: &Entry, snap: Option<&Snapshot>, sparks: u32) -> Row {
    let recipes = snap.map(|s| s.recipes.as_slice()).unwrap_or(&[]);
    let matches = |r: &&crate::deck::RecipeView| {
        r.name == e.name || r.source_repo.as_deref().is_some_and(|x| norm_repo(x) == norm_repo(&e.repo))
    };
    let deck = recipes.iter().filter(|r| r.origin == crate::catalog::DECK).find(matches);
    let builtin = recipes.iter().filter(|r| r.origin == crate::catalog::BUILTIN).find(matches);
    let (installed, rv) = match (deck, builtin) {
        (Some(r), _) => ("deck", Some(r)),
        (None, Some(r)) => ("builtin", Some(r)),
        _ => ("no", None),
    };
    let update = rv.and_then(|r| {
        if let Some(u) = r.update.as_ref().filter(|u| u.available()) {
            return Some(format!("⬆ {} upstream ({})", u.delta(), u.hint(&r.name).unwrap_or_default()));
        }
        let (Some(want), Some(have)) = (e.git_ref.as_deref(), r.source_ref.as_deref()) else { return None };
        let same =
            want == have || (want.len() >= 7 && have.starts_with(want)) || (have.len() >= 7 && want.starts_with(have));
        (!same).then(|| {
            format!(
                "⬆ index pins {} (you have {}): lmdeck add {} --force",
                crate::updates::short(want),
                crate::updates::short(have),
                e.name
            )
        })
    });
    let (fit, fit_reason) = fit(e, snap, rv, sparks);
    let install = match installed {
        "deck" => format!("in your deck as {}", rv.map(|r| r.name.as_str()).unwrap_or(&e.name)),
        _ => format!("lmdeck add {}", e.name),
    };
    Row {
        entry: e.clone(),
        installed,
        installed_as: rv.map(|r| r.name.clone()),
        update,
        fit,
        fit_reason,
        readme: e.readme_url(),
        install,
    }
}

fn fit(e: &Entry, snap: Option<&Snapshot>, rv: Option<&crate::deck::RecipeView>, sparks: u32) -> (Level, String) {
    let need = e.sparks();
    if need > 1 && need > sparks {
        return (Level::Fail, format!("{sparks} of {need} Sparks online (add the others with `lmdeck setup`)"));
    }
    if let Some(r) = rv {
        let why = r.checks.iter().filter(|c| c.level != Level::Ok).map(|c| c.detail.clone()).collect::<Vec<_>>();
        return (r.fit, if why.is_empty() { "fits this machine".into() } else { why.join("; ") });
    }
    let Some(hw) = snap.map(|s| &s.hardware) else { return (Level::Warn, "unknown hardware".into()) };
    let spark = hw.platform.to_lowercase().contains("gb10") || hw.platform.to_lowercase().contains("spark");
    if let Some(gb) = e.size_gb {
        let have = hw.total_memory_gb * need as f64;
        if gb > have {
            return (
                Level::Fail,
                if need == 1 {
                    format!("~{gb:.0} GB of weights; this machine has {have:.0} GB")
                } else {
                    format!("~{gb:.0} GB of weights; {have:.0} GB across {need} machines like this one")
                },
            );
        }
        if gb > have * 0.85 {
            return (Level::Warn, format!("~{gb:.0} GB of weights; tight in {have:.0} GB"));
        }
    }
    if e.tags.sparks.is_some() && !spark {
        return (Level::Warn, format!("made for DGX Spark; this is {}", hw.platform));
    }
    if need > 1 {
        return (Level::Ok, format!("{need} Sparks · {sparks} online"));
    }

    (Level::Ok, "fits this machine".into())
}

/// Search (words over name, description, repo and tags; fuzzy on the name), tag filters (every
/// one must match, `value` or `key=value`), then the sort.
pub fn filter<'a>(rows: &'a [Row], query: &str, tags: &[String], sort: Sort) -> Vec<&'a Row> {
    let mut v: Vec<(i64, &Row)> = rows
        .iter()
        .filter(|r| tags.iter().all(|t| r.entry.has_tag(t)))
        .filter_map(|r| if query.trim().is_empty() { Some((0, r)) } else { matches(query, &r.entry).map(|s| (s, r)) })
        .collect();
    v.sort_by(|(sa, a), (sb, b)| {
        let by = match sort {
            Sort::Name => std::cmp::Ordering::Equal,
            Sort::Sparks => a.entry.sparks().cmp(&b.entry.sparks()),
            Sort::Speed => {
                let s = |r: &Row| r.entry.speed.as_ref().map(|s| s.tok_s).unwrap_or(-1.0);
                s(b).partial_cmp(&s(a)).unwrap_or(std::cmp::Ordering::Equal)
            }
        };
        let q = if query.trim().is_empty() { std::cmp::Ordering::Equal } else { sb.cmp(sa) };
        by.then(q).then_with(|| a.entry.name.cmp(&b.entry.name))
    });
    v.into_iter().map(|(_, r)| r).collect()
}

/// Every query word appears in the entry's text (name, description, repo,
/// tags), or the query is a fuzzy match on the name. Higher is better.
fn matches(query: &str, e: &Entry) -> Option<i64> {
    let q = query.trim().to_lowercase();
    let text = e.search_text().to_lowercase();
    let name = e.name.to_lowercase();
    let fuzzy = crate::tui::app::fuzzy_score(&q, &e.name);
    if q.split_whitespace().all(|w| text.contains(w)) {
        let bonus = if name.contains(&q) { 1000 } else { 0 };
        return Some(bonus + fuzzy.unwrap_or(0));
    }
    fuzzy
}

/// Every tag value in the index, for chips: `(key, value, count)`.
pub fn tag_values(index: &Index) -> Vec<(String, String, usize)> {
    let mut m: std::collections::BTreeMap<(String, String), usize> = Default::default();
    for e in &index.entries {
        for (k, v) in e.tags.pairs() {
            *m.entry((k.to_string(), v)).or_default() += 1;
        }
    }
    m.into_iter().map(|((k, v), n)| (k, v, n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> Index {
        Index::parse(
            r#"
[[entry]]
name = "zeta-big"
repo = "https://github.com/o/zeta"
description = "two sparks"
size_gb = 176.0
tags = { engine = "tensorfold", quant = "exl3", sparks = 2 }
speed = { tok_s = 60.4, note = "README" }
[[entry]]
name = "alpha-small"
repo = "https://github.com/o/alpha"
description = "one spark"
tags = { engine = "vllm", quant = "nvfp4", sparks = 1 }
speed = { tok_s = 90.0, note = "README" }
[[entry]]
name = "mid-nospeed"
repo = "https://github.com/o/mid"
description = "llama"
tags = { engine = "llama.cpp", quant = "gguf" }
"#,
        )
        .unwrap()
    }

    fn names(v: &[&Row]) -> Vec<String> {
        v.iter().map(|r| r.entry.name.clone()).collect()
    }

    #[test]
    fn filter_and_sort() {
        let rows = rows(&idx(), None, 1);
        assert_eq!(names(&filter(&rows, "", &[], Sort::Name)), ["alpha-small", "mid-nospeed", "zeta-big"]);
        assert_eq!(names(&filter(&rows, "", &[], Sort::Speed)), ["alpha-small", "zeta-big", "mid-nospeed"]);
        assert_eq!(names(&filter(&rows, "", &[], Sort::Sparks)), ["alpha-small", "mid-nospeed", "zeta-big"]);
        assert_eq!(names(&filter(&rows, "", &["exl3".into()], Sort::Name)), ["zeta-big"]);
        assert_eq!(names(&filter(&rows, "", &["sparks=1".into()], Sort::Name)), ["alpha-small"]);
        assert!(filter(&rows, "", &["engine=vllm".into(), "quant=gguf".into()], Sort::Name).is_empty());
        assert_eq!(names(&filter(&rows, "llama", &[], Sort::Name)), ["mid-nospeed"]);
        assert_eq!(names(&filter(&rows, "spark", &["tensorfold".into()], Sort::Name)), ["zeta-big"]);
        assert_eq!(Sort::parse("Speed"), Some(Sort::Speed));
        assert_eq!(Sort::Speed.next(), Sort::Name);
        let tags = tag_values(&idx());
        assert!(tags.contains(&("engine".into(), "vllm".into(), 1)));
        assert!(tags.contains(&("sparks".into(), "2".into(), 1)));
    }

    #[test]
    fn two_spark_entries_need_two_sparks_online() {
        let i = idx();
        let one = rows(&i, None, 1);
        let z = one.iter().find(|r| r.entry.name == "zeta-big").unwrap();
        assert_eq!(z.fit, Level::Fail);
        assert!(z.fit_reason.contains("1 of 2 Sparks online"), "{}", z.fit_reason);
        assert_eq!(z.installed, "no");
        assert_eq!(z.install, "lmdeck add zeta-big");
        assert_eq!(z.readme, "https://github.com/o/zeta#readme");
        let two = rows(&i, None, 2);
        assert_ne!(two.iter().find(|r| r.entry.name == "zeta-big").unwrap().fit_reason, z.fit_reason);
        // A Mac controller (no Spark online) can still run single-machine entries.
        let none = rows(&i, None, 0);
        assert!(none.iter().filter(|r| r.entry.sparks() == 1).all(|r| !r.fit_reason.contains("online")));
    }
}
