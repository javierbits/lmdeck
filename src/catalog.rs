//! All recipes lmdeck can see: the deck, the built-in examples, and sources.

use anyhow::{bail, Result};
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::paths::Paths;
use crate::recipe::{is_not_a_recipe, is_recipe_file, is_yaml_file, Recipe};
use crate::sources::{load_dir, Found};

pub const DECK: &str = "deck";
pub const BUILTIN: &str = "builtin";
/// Recipes a controller uploaded to this machine (`POST /api/run`), kept in
/// `<state>/remote-recipes` so status, logs and stop work by name.
pub const REMOTE: &str = "remote";

pub fn remote_dir(paths: &Paths) -> std::path::PathBuf {
    paths.state_dir.join("remote-recipes")
}

/// Saves an uploaded recipe (replacing an older upload of the same name).
pub fn save_remote(paths: &Paths, recipe: &Recipe) -> Result<std::path::PathBuf> {
    let dir = remote_dir(paths);
    fs::create_dir_all(&dir)?;
    let p = dir.join(format!("{}.toml", recipe.name));
    fs::write(&p, toml::to_string(recipe)?)?;
    Ok(p)
}

/// Example recipes compiled into the binary.
pub const BUILTIN_RECIPES: &[(&str, &str)] = &[
    ("qwen3-4b-gguf.toml", include_str!("../recipes/qwen3-4b-gguf.toml")),
    ("gpt-oss-20b-gguf.toml", include_str!("../recipes/gpt-oss-20b-gguf.toml")),
    ("qwen3-30b-a3b-exl3.toml", include_str!("../recipes/qwen3-30b-a3b-exl3.toml")),
    ("qwen3.6-35b-a3b-nvfp4-spark.toml", include_str!("../recipes/qwen3.6-35b-a3b-nvfp4-spark.toml")),
    ("qwen3-4b-mlx.toml", include_str!("../recipes/qwen3-4b-mlx.toml")),
    ("qwen3-4b-lmstudio.toml", include_str!("../recipes/qwen3-4b-lmstudio.toml")),
    ("qwen3.5-2b-tensorfold.toml", include_str!("../recipes/qwen3.5-2b-tensorfold.toml")),
    ("qwen3.6-35b-a3b-mtp-tensorfold-spark.toml", include_str!("../recipes/qwen3.6-35b-a3b-mtp-tensorfold-spark.toml")),
    ("ornith-1.5-122b-tensorfold.toml", include_str!("../recipes/ornith-1.5-122b-tensorfold.toml")),
    (
        "deepseek-v4.1-flash-1017-4x-spark-tensorfold.toml",
        include_str!("../recipes/deepseek-v4.1-flash-1017-4x-spark-tensorfold.toml"),
    ),
];

pub fn builtin() -> Vec<Found> {
    BUILTIN_RECIPES
        .iter()
        .map(|(file, text)| {
            let stem = file.trim_end_matches(".toml");
            let recipe = Recipe::parse(text, stem).expect("builtin recipes are valid");
            Found { recipe, origin: BUILTIN.into(), file: (*file).into() }
        })
        .collect()
}

/// Recipes in the deck (`~/.config/lmdeck/recipes`), sorted by name.
pub fn deck(paths: &Paths) -> (Vec<Found>, Vec<(String, String)>) {
    plain_dir(DECK, &paths.recipes_dir())
}

/// Every `.toml` recipe directly in `dir`, sorted by name.
fn plain_dir(origin: &str, dir: &Path) -> (Vec<Found>, Vec<(String, String)>) {
    let mut found = vec![];
    let mut errors = vec![];
    let Ok(rd) = fs::read_dir(dir) else { return (found, errors) };
    let mut files: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    for path in files {
        // A leftover `.yaml` is reported (YAML is no longer read), not skipped.
        if !(is_recipe_file(&path) || is_yaml_file(&path)) {
            continue;
        }
        match Recipe::load(&path) {
            Ok(recipe) => found.push(Found { recipe, origin: origin.into(), file: path.display().to_string() }),
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
        let (remote, _) = plain_dir(REMOTE, &remote_dir(paths));
        entries.extend(remote);
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
    let p = Path::new(s);
    p.is_file() && (is_recipe_file(p) || is_yaml_file(p))
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

    #[test]
    fn yaml_in_the_deck_is_an_error() {
        let (_d, paths) = tmp();
        fs::create_dir_all(paths.recipes_dir()).unwrap();
        fs::write(paths.recipes_dir().join("old.yaml"), "runtime: llama\nmodel: a/b/c.gguf\n").unwrap();
        fs::write(paths.recipes_dir().join("notes.txt"), "x").unwrap();
        let (found, errors) = deck(&paths);
        assert!(found.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.contains("YAML recipes are no longer supported"), "{:?}", errors);
    }

    #[test]
    fn ornith_wrapper_builtin() {
        let r = builtin().into_iter().find(|f| f.recipe.name == "ornith-1.5-122b-tensorfold").unwrap().recipe;
        let e = &r.engine;
        assert_eq!(e.name, crate::recipe::Engine::Custom);
        assert!(e.detached);
        assert_eq!(e.container.as_deref(), Some("ornith-122b-tf"));
        assert_eq!(r.port(), 8888);
        assert_eq!(e.command, ["./start.sh"]);
        assert_eq!(e.stop, ["./stop.sh"]);
        assert_eq!(e.logs, ["docker", "logs", "-f", "ornith-122b-tf"]);
        assert!(e.ready_timeout.unwrap() >= 3600);
        assert!(r.source.unwrap().repo.contains("MiaAI-Lab/Ornith-1.5-122B-TensorFold"));
    }

    #[test]
    fn deepseek_v41_wrapper_is_one_local_command() {
        let r = builtin()
            .into_iter()
            .find(|f| f.recipe.name == "deepseek-v4.1-flash-1017-4x-spark-tensorfold")
            .unwrap()
            .recipe;
        let e = &r.engine;
        assert_eq!(e.name, crate::recipe::Engine::Custom);
        assert!(e.detached);
        assert_eq!(e.container.as_deref(), Some("dsv41-1017-tf"));
        assert_eq!(r.port(), 8888);
        assert_eq!(e.command, ["./start.sh"]);
        assert_eq!(e.stop, ["./stop.sh"]);
        assert_eq!(e.logs, ["docker", "logs", "-f", "dsv41-1017-tf"]);
        assert_eq!(e.ready_timeout, Some(43200));
        assert_eq!(e.ctx, Some(1_048_576));
        // Mia's start.sh ssh's the workers. Setting hardware.nodes without [[nodes]]
        // makes `lmdeck use` refuse the recipe.
        assert!(r.hardware.nodes.is_none());
        assert_eq!(r.nodes(), 1);
        assert!(r.nodes.is_empty());
        assert_eq!(r.family(), "DeepSeek");
        let src = r.source.unwrap();
        assert!(src.repo.contains("MiaAI-Lab/DeepSeek-V4.1-Flash-1017-4x-DGX-Sparks-TensorFold"));
        assert_eq!(src.git_ref.as_deref(), Some("476bbc77c4e1d1abe45b1cd095526a9ea0d36da4"));
    }

    #[test]
    fn recipe_schemas_missing_current_newer() {
        // Every built-in and example says `schema = 1` itself.
        for (file, text) in BUILTIN_RECIPES {
            assert!(text.lines().any(|l| l.trim() == "schema = 1"), "{file} has no schema line");
        }
        for ex in ["ornith-122b-awq-lmdeploy.toml", "vllm-ray-2spark.toml"] {
            let t = fs::read_to_string(format!("{}/examples/{ex}", env!("CARGO_MANIFEST_DIR"))).unwrap();
            assert!(t.lines().any(|l| l.trim() == "schema = 1"), "{ex}");
        }
        let body = "[engine]\nname = \"llama.cpp\"\n[model]\nrepo = \"a/b\"\nfiles = [\"m.gguf\"]\n";
        assert_eq!(Recipe::parse(body, "old").unwrap().schema, 1, "missing means 1");
        assert_eq!(Recipe::parse(&format!("schema = 1\n{body}"), "cur").unwrap().schema, 1);
        let r = Recipe::parse(body, "old").unwrap();
        assert!(r.to_toml().unwrap().starts_with("schema = 1\n"), "written files carry it");
        let (_d, paths) = tmp();
        fs::create_dir_all(paths.recipes_dir()).unwrap();
        fs::write(paths.recipes_dir().join("old.toml"), body).unwrap();
        fs::write(paths.recipes_dir().join("future.toml"), format!("schema = 2\nmin_lmdeck = \"0.3.0\"\n{body}"))
            .unwrap();
        let (found, errors) = deck(&paths);
        assert_eq!(found.iter().map(|f| f.recipe.name.as_str()).collect::<Vec<_>>(), ["old"]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.contains(crate::schema::MARKER), "{errors:?}");
        assert!(errors[0].1.contains("needs lmdeck >= 0.3.0 / schema 2"), "{errors:?}");
    }
}
