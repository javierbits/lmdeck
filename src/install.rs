//! `lmdeck add <repo>` and `lmdeck recipe add <ref>`, shared by the CLI and
//! the dashboard (`lmdeck serve` cannot call into `main.rs`).
//!
//! The functions write the recipe into the deck; they never start anything.
//! Progress goes to the caller's two sinks so the CLI can print it and the
//! dashboard can return it as JSON lines.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::catalog;
use crate::checkout;
use crate::config::Config;
use crate::engine;
use crate::index;
use crate::paths::{expand, Paths};
use crate::recipe::{is_recipe_file, is_yaml_file, valid_name, GitSource, Recipe, YAML_UNSUPPORTED};
use crate::ui::{bold, cyan, dim, green, yellow};
use crate::wrap;

/// What an install added.
#[derive(Debug, Clone, PartialEq)]
pub struct Added {
    pub name: String,
    pub path: PathBuf,
}

/// Where progress lines go: `say` for normal output, `warn` for something
/// the user should look at.
pub struct Reply<'a> {
    pub say: &'a mut dyn FnMut(&str),
    pub warn: &'a mut dyn FnMut(&str),
}

impl Reply<'_> {
    fn say(&mut self, s: String) {
        (self.say)(&s);
    }
    fn warn(&mut self, s: String) {
        (self.warn)(&s);
    }
}

/// `lmdeck add <repo>`: a bare name is looked up in the recipe index; anything
/// else is treated as a repo and wrapped (its own `lmdeck.toml` when it has
/// one, else a `start.sh` / `stop.sh` wrapper).
pub fn add(
    paths: &Paths,
    cfg: &Config,
    repo: &str,
    name: Option<String>,
    git_ref: Option<String>,
    force: bool,
    reply: &mut Reply<'_>,
) -> Result<Added> {
    if is_index_name(repo) {
        return add_from_index(paths, cfg, repo, name, git_ref, force, reply);
    }
    let (url, url_ref, default_name) = wrap::parse_repo_arg(repo)?;
    // A repo a built-in recipe already wraps (tuned by hand) gets that recipe.
    if name.is_none() && git_ref.is_none() && url_ref.is_none() {
        let same = |r: &str| r.trim_end_matches('/').trim_end_matches(".git").eq_ignore_ascii_case(&url);
        let built = catalog::builtin().into_iter().find(|f| f.recipe.source.as_ref().is_some_and(|s| same(&s.repo)));
        if let Some(f) = built {
            reply.say(format!(
                "{} the built-in recipe {} already wraps this repo; adding that copy {}",
                cyan("◆"),
                bold(&f.recipe.name),
                dim("(pass --name to write a fresh wrapper instead)")
            ));
            let path = catalog::add_to_deck(paths, &f.recipe, force)?;
            added_to_deck(reply, &f.recipe.name, &path, None);
            return Ok(Added { name: f.recipe.name, path });
        }
    }
    let name = name.unwrap_or(default_name);
    if !valid_name(&name) {
        bail!("invalid name '{name}': use letters, digits, '.', '_' or '-' (or pass --name)");
    }
    let deck_file = paths.recipes_dir().join(format!("{name}.toml"));
    if deck_file.exists() && !force {
        bail!("{name} is already in the deck ({}); use --force to overwrite", deck_file.display());
    }
    let wanted_ref = git_ref.or(url_ref);
    let co = engine::checkout_dir(&paths.checkouts_dir(), &name);
    if co.join(".git").exists() {
        let origin = checkout::origin_url(&co).unwrap_or_default();
        if origin != url {
            if !force {
                bail!("{} already holds a checkout of {origin}; use --force to replace it", co.display());
            }
            std::fs::remove_dir_all(&co).with_context(|| format!("removing {}", co.display()))?;
        }
    }
    reply.say(format!("{} {} {}", cyan("↓"), url, dim(&format!("→ {}", co.display()))));
    let src = GitSource { repo: url.clone(), git_ref: wanted_ref.clone(), branch: None };
    let outcome = checkout::sync(&src, &co, true)?;
    let branch = checkout::default_branch(&co);
    // Pin the commit unless a ref was asked for.
    let pinned = wanted_ref.clone().unwrap_or_else(|| outcome.commit().to_string());
    let display = url.rsplit(['/', ':']).next().unwrap_or(&name).trim_end_matches(".git").to_string();
    let pin = wrap::SourcePin {
        repo: &url,
        git_ref: &pinned,
        branch: if wanted_ref.is_none() { branch.as_deref() } else { None },
    };
    // The repo's own native recipe wins over wrapper detection.
    if let Some(recipe) = wrap::from_manifest(&co, &name, &pin)? {
        let path = catalog::add_to_deck(paths, &recipe, force)?;
        added_to_deck(reply, &name, &path, Some(&format!("from {}", wrap::MANIFEST)));
        return Ok(Added { name, path });
    }
    let det = wrap::detect(&co, &url)?;
    let text = wrap::recipe_toml(&name, &display, &pin, &det);
    Recipe::parse(&text, &name).context("the generated recipe does not parse (please report this)")?;
    std::fs::create_dir_all(paths.recipes_dir())?;
    std::fs::write(&deck_file, &text).with_context(|| format!("writing {}", deck_file.display()))?;
    added_to_deck(reply, &name, &deck_file, None);
    let found = |reply: &mut Reply<'_>, label: &str, v: Option<String>| match v {
        Some(v) => reply.say(format!("  {label:<10} {v}")),
        None => reply.say(format!("  {label:<10} {}", yellow("not found (TODO in the recipe)"))),
    };
    found(reply, "port", det.port.map(|p| format!("{p} (from {})", det.port_var.clone().unwrap_or_default())));
    if det.uses_docker || det.container.is_some() {
        found(reply, "container", det.container.clone());
    }
    reply.say(format!("  {:<10} {}", "detached", if det.detached { "yes" } else { "no (TODO: check)" }));
    for (k, s) in &det.by_fabric {
        reply.say(format!("  {:<10} ./{s} on a {k}", "launcher"));
    }
    if let Some(n) = det.hosts {
        reply.say(format!("  {:<10} {n} Sparks (its scripts reach the others)", "needs"));
    }
    reply.say(format!("  {:<10} {}s", "ready in", wrap::ready_timeout(&det)));
    if let Some(note) = wrap::script_note(&det, &deck_file) {
        reply.warn(note);
    }

    let todos = text.matches("TODO").count();
    if todos > 0 {
        reply.warn(format!("{todos} TODO(s) to review in {}", deck_file.display()));
    }
    reply.say(format!("  try: lmdeck use {name} --dry-run"));
    Ok(Added { name, path: deck_file })
}

/// `lmdeck recipe add <reference>`: a file, a raw TOML URL, or a recipe from
/// the deck / built-ins / sources.
pub fn add_recipe(paths: &Paths, cfg: &Config, reference: &str, name: Option<String>, force: bool) -> Result<Added> {
    let mut recipe = if catalog::is_file_ref(reference) {
        Recipe::load(Path::new(reference))?
    } else if reference.starts_with("https://") || reference.starts_with("http://") {
        let path_part = reference.split(['?', '#']).next().unwrap_or(reference);
        if is_yaml_file(Path::new(path_part)) {
            bail!("{YAML_UNSUPPORTED}");
        }
        if !is_recipe_file(Path::new(path_part)) {
            bail!("URL must end in .toml (to wrap a repo's start.sh / stop.sh, use `lmdeck add <repo>`)");
        }
        let stem = Path::new(path_part).file_stem().and_then(|s| s.to_str()).unwrap_or("recipe").to_string();
        Recipe::parse(&fetch_url(reference)?, &stem)?
    } else {
        let cat = catalog::Catalog::load(paths, cfg);
        let f = cat.resolve(reference)?;
        if f.origin == catalog::DECK && name.is_none() {
            bail!("{} is already in the deck", f.recipe.name);
        }
        f.recipe.clone()
    };
    if let Some(n) = name {
        if !valid_name(&n) {
            bail!("invalid name '{n}': use letters, digits, '.', '_' or '-'");
        }
        recipe.name = n;
    }
    let path = catalog::add_to_deck(paths, &recipe, force)?;
    Ok(Added { name: recipe.name, path })
}

fn added_to_deck(reply: &mut Reply<'_>, name: &str, path: &Path, from: Option<&str>) {
    let from = from.map(|f| format!(" {}", dim(f))).unwrap_or_default();
    reply.say(format!(
        "{} added {} to the deck {}{}",
        green("✓"),
        bold(name),
        dim(&format!("({})", path.display())),
        from
    ));
    if from.is_empty() {
        reply.say(format!("  try: lmdeck use {name} --dry-run"));
    }
}

/// A bare name (no `/`, `:` and not an existing directory) is an index name.
fn is_index_name(arg: &str) -> bool {
    let a = arg.trim();
    !a.is_empty() && !a.contains(['/', ':', '\\']) && !a.starts_with('~') && !expand(a).is_dir()
}

fn add_from_index(
    paths: &Paths,
    cfg: &Config,
    arg: &str,
    name: Option<String>,
    git_ref: Option<String>,
    force: bool,
    reply: &mut Reply<'_>,
) -> Result<Added> {
    let loaded = index::load(paths, &cfg.index, false);
    if let Some(h) = &loaded.hint {
        reply.warn(h.clone());
    }
    let Some(e) = loaded.index.find(arg) else {
        let mut near: Vec<(i64, &str)> = loaded
            .index
            .entries
            .iter()
            .filter_map(|e| crate::tui::app::fuzzy_score(arg, &e.name).map(|s| (s, e.name.as_str())))
            .collect();
        near.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        let near: Vec<&str> = near.into_iter().take(3).map(|x| x.1).collect();
        let more = if near.is_empty() {
            format!("{} entries in the index; see `lmdeck browse`", loaded.index.entries.len())
        } else {
            format!("did you mean: {}?", near.join(", "))
        };
        bail!("'{arg}' is not in the recipe index ({more}); for a repo pass owner/repo or a URL");
    };
    let e = e.clone();
    reply.say(format!("{} {} {}", cyan("◆"), bold(&e.name), dim(&format!("from the index ({})", loaded.url))));
    if e.recipe.is_some() && git_ref.is_none() {
        // A recipe that ships inside this binary needs no network at all.
        let mut r = match catalog::builtin().into_iter().find(|f| f.recipe.name == e.name) {
            Some(f) => {
                reply.say(format!("  {}", dim("using the built-in copy that ships with lmdeck")));
                f.recipe
            }
            None => index::fetch_recipe(&cfg.index, &e)?.ok_or_else(|| anyhow!("{} has no recipe file", e.name))?,
        };

        if let Some(n) = name {
            if !valid_name(&n) {
                bail!("invalid name '{n}'");
            }
            r.name = n;
        }
        let path = catalog::add_to_deck(paths, &r, force)?;
        added_to_deck(reply, &r.name, &path, None);
        return Ok(Added { name: r.name, path });
    }
    let wanted = name.unwrap_or_else(|| e.name.clone());
    add(paths, cfg, &e.repo, Some(wanted), git_ref.or_else(|| e.git_ref.clone()), force, reply)
}

/// Fetches a raw recipe file. GitHub and Hugging Face blob URLs are rewritten
/// to their raw/resolve forms.
pub fn fetch_url(url: &str) -> Result<String> {
    let url = if let Some(rest) = url.strip_prefix("https://github.com/") {
        format!("https://raw.githubusercontent.com/{}", rest.replacen("/blob/", "/", 1))
    } else if url.starts_with("https://huggingface.co/") {
        url.replacen("/blob/", "/resolve/", 1)
    } else {
        url.to_string()
    };
    let resp = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
        .get(&url)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    Ok(resp.into_string()?)
}
