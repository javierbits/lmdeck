//! All recipes lmdeck can see: the deck, the built-in examples, and sources.

use anyhow::{bail, Result};
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::paths::Paths;
use crate::recipe::{is_not_a_recipe, Format, Recipe};
use crate::sources::{load_dir, Found};

pub const DECK: &str = "deck";
pub const BUILTIN: &str = "builtin";

/// Example recipes compiled into the binary.
pub const BUILTIN_RECIPES: &[(&str, &str)] = &[
    ("qwen3-4b-gguf.toml", include_str!("../recipes/qwen3-4b-gguf.toml")),
    ("gpt-oss-20b-gguf.toml", include_str!("../recipes/gpt-oss-20b-gguf.toml")),
    ("qwen3-30b-a3b-exl3.toml", include_str!("../recipes/qwen3-30b-a3b-exl3.toml")),
    ("qwen3.6-35b-a3b-nvfp4-spark.toml", include_str!("../recipes/qwen3.6-35b-a3b-nvfp4-spark.toml")),
    ("qwen3-4b-mlx.toml", include_str!("../recipes/qwen3-4b-mlx.toml")),
    ("qwen3-4b-lmstudio.toml", include_str!("../recipes/qwen3-4b-lmstudio.toml")),
];

pub fn builtin() -> Vec<Found> {
    BUILTIN_RECIPES
        .iter()
        .map(|(file, text)| {
            let stem = file.trim_end_matches(".toml");
            let recipe = Recipe::parse(text, Format::Toml, stem).expect("builtin recipes are valid");
            Found { recipe, origin: BUILTIN.into(), file: (*file).into() }
        })
        .collect()
}

/// Recipes in the deck (`~/.config/lmdeck/recipes`), sorted by name.
pub fn deck(paths: &Paths) -> (Vec<Found>, Vec<(String, String)>) {
    let mut found = vec![];
    let mut errors = vec![];
    let Ok(rd) = fs::read_dir(paths.recipes_dir()) else { return (found, errors) };
    let mut files: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    for path in files {
        if Format::from_path(&path).is_none() {
            continue;
        }
        match Recipe::load(&path) {
            Ok(recipe) => found.push(Found { recipe, origin: DECK.into(), file: path.display().to_string() }),
            Err(e) if is_not_a_recipe(&e) => {}
            Err(e) => errors.push((path.display().to_string(), format!("{e:#}"))),
        }
    }
    found.sort_by(|a, b| a.recipe.name.cmp(&b.recipe.name));
    (found, errors)
}

/// Every recipe from every origin, deck first.
pub struct Catalog {
    pub entries: Vec<Found>,
    pub errors: Vec<(String, String)>,
}

impl Catalog {
    pub fn load(paths: &Paths, cfg: &Config) -> Catalog {
        let (mut entries, mut errors) = deck(paths);
        entries.extend(builtin());
        for s in &cfg.sources {
            let (f, e) = load_dir(&s.name, &paths.sources_cache_dir().join(&s.name));
            entries.extend(f);
            errors.extend(e.into_iter().map(|(file, err)| (format!("{}/{file}", s.name), err)));
        }
        Catalog { entries, errors }
    }

    /// Resolves `name` or `origin/name`. A bare name prefers the deck, and is
    /// ambiguous only when several non-deck origins share it.
    pub fn resolve(&self, reference: &str) -> Result<&Found> {
        if let Some((origin, name)) = reference.split_once('/') {
            if let Some(f) = self.entries.iter().find(|f| f.origin == origin && f.recipe.name == name) {
                return Ok(f);
            }
        }
        let matches: Vec<&Found> = self.entries.iter().filter(|f| f.recipe.name == reference).collect();
        if let Some(f) = matches.iter().find(|f| f.origin == DECK) {
            return Ok(f);
        }
        match matches.len() {
            0 => {
                let lower = reference.to_lowercase();
                let close: Vec<String> = self
                    .entries
                    .iter()
                    .filter(|f| f.recipe.name.to_lowercase().contains(&lower))
                    .take(5)
                    .map(|f| format!("{}/{}", f.origin, f.recipe.name))
                    .collect();
                if close.is_empty() {
                    bail!("no recipe named '{reference}' (try `lmdeck recipe search`)")
                }
                bail!("no recipe named '{reference}'; did you mean: {}", close.join(", "))
            }
            1 => Ok(matches[0]),
            _ => bail!(
                "'{reference}' is ambiguous: {}",
                matches.iter().map(|f| format!("{}/{}", f.origin, f.recipe.name)).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    pub fn deck(&self) -> impl Iterator<Item = &Found> {
        self.entries.iter().filter(|f| f.origin == DECK)
    }
}

/// Saves a recipe into the deck as TOML. Fails if it exists unless `force`.
pub fn add_to_deck(paths: &Paths, recipe: &Recipe, force: bool) -> Result<std::path::PathBuf> {
    let dir = paths.recipes_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.toml", recipe.name));
    if path.exists() && !force {
        bail!("{} is already in the deck ({}); use --force to overwrite", recipe.name, path.display());
    }
    // Remove a same-named YAML so the deck has one file per recipe.
    for ext in ["yaml", "yml"] {
        let other = dir.join(format!("{}.{ext}", recipe.name));
        if other.exists() && force {
            fs::remove_file(other)?;
        }
    }
    let header = format!("# lmdeck recipe '{}'\n", recipe.name);
    fs::write(&path, header + &recipe.to_toml()?)?;
    Ok(path)
}

pub fn remove_from_deck(paths: &Paths, name: &str) -> Result<bool> {
    let mut removed = false;
    for ext in ["toml", "yaml", "yml"] {
        let p = paths.recipes_dir().join(format!("{name}.{ext}"));
        if p.exists() {
            fs::remove_file(p)?;
            removed = true;
        }
    }
    Ok(removed)
}

/// Is `s` a path to an existing recipe file?
pub fn is_file_ref(s: &str) -> bool {
    Path::new(s).is_file() && Format::from_path(Path::new(s)).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, Paths) {
        let d = tempfile::tempdir().unwrap();
        let p = Paths { config_dir: d.path().join("c"), cache_dir: d.path().join("k"), state_dir: d.path().join("s") };
        (d, p)
    }

    #[test]
    fn builtins_parse_and_are_unique() {
        let b = builtin();
        assert_eq!(b.len(), BUILTIN_RECIPES.len());
        let mut names: Vec<_> = b.iter().map(|f| f.recipe.name.clone()).collect();
        names.dedup();
        assert_eq!(names.len(), b.len());
        assert!(b.iter().all(|f| f.recipe.quant.is_some()));
    }

    #[test]
    fn deck_add_resolve_remove() {
        let (_d, paths) = tmp();
        let cfg = Config { sources: vec![], ..Config::default() };
        let mut r = builtin().remove(0).recipe;
        add_to_deck(&paths, &r, false).unwrap();
        assert!(add_to_deck(&paths, &r, false).is_err());
        r.description = Some("mine".into());
        add_to_deck(&paths, &r, true).unwrap();
        let cat = Catalog::load(&paths, &cfg);
        let f = cat.resolve(&r.name).unwrap();
        assert_eq!(f.origin, DECK);
        assert_eq!(f.recipe.description.as_deref(), Some("mine"));
        assert_eq!(cat.resolve(&format!("builtin/{}", r.name)).unwrap().origin, BUILTIN);
        assert!(cat.resolve("nope-nothing").is_err());
        let err = cat.resolve("qwen3").unwrap_err().to_string();
        assert!(err.contains("did you mean"), "{err}");
        assert!(remove_from_deck(&paths, &r.name).unwrap());
        assert!(!remove_from_deck(&paths, &r.name).unwrap());
    }
}
