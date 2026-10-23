//! XDG-style directory resolution.
//!
//! lmdeck deliberately uses XDG locations on every Unix (including macOS) so
//! that `~/.config/lmdeck` is the one place to look, on a DGX Spark or a Mac:
//!
//! | what    | env override        | default                    |
//! |---------|---------------------|----------------------------|
//! | config  | `XDG_CONFIG_HOME`   | `~/.config/lmdeck`         |
//! | cache   | `XDG_CACHE_HOME`    | `~/.cache/lmdeck`          |
//! | state   | `XDG_STATE_HOME`    | `~/.local/state/lmdeck`    |
//!
//! `LMDECK_HOME`, when set, roots all three under one directory (handy for
//! tests and for running several isolated decks).

use std::env;
use std::path::{Path, PathBuf};

/// Resolved lmdeck directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    /// Resolves directories from the current process environment.
    pub fn from_env() -> Self {
        Self::resolve(|k| env::var(k).ok(), home_dir())
    }

    /// Pure resolution logic, parameterised over an env lookup for testing.
    pub fn resolve(get: impl Fn(&str) -> Option<String>, home: PathBuf) -> Self {
        let nonempty = |k: &str| get(k).filter(|v| !v.trim().is_empty()).map(PathBuf::from);
        if let Some(root) = nonempty("LMDECK_HOME") {
            return Paths {
                config_dir: root.join("config"),
                cache_dir: root.join("cache"),
                state_dir: root.join("state"),
            };
        }
        let config = nonempty("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let cache = nonempty("XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache"));
        let state = nonempty("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local").join("state"));
        Paths { config_dir: config.join("lmdeck"), cache_dir: cache.join("lmdeck"), state_dir: state.join("lmdeck") }
    }

    /// `config.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// The deck: recipes the user has added.
    pub fn recipes_dir(&self) -> PathBuf {
        self.config_dir.join("recipes")
    }

    /// Mirror of each recipe source, one sub-directory per source.
    pub fn sources_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("sources")
    }

    /// Run records (`<name>.json`) for models lmdeck started.
    /// `[source]` git checkouts for wrapped recipes, one per recipe.
    pub fn checkouts_dir(&self) -> PathBuf {
        self.cache_dir.join("checkouts")
    }

    pub fn run_dir(&self) -> PathBuf {
        self.state_dir.join("run")
    }

    /// Server logs (`<name>.log`).
    pub fn log_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }
}

/// The user's home directory (`$HOME`), falling back to `/`.
pub fn home_dir() -> PathBuf {
    env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Expands a leading `~` / `~/` and `$VAR` / `${VAR}` references.
pub fn expand(input: &str) -> PathBuf {
    expand_with(input, |k| env::var(k).ok(), &home_dir())
}

/// Pure version of [`expand`].
pub fn expand_with(input: &str, get: impl Fn(&str) -> Option<String>, home: &Path) -> PathBuf {
    let mut s = String::new();
    let rest = if input == "~" {
        s.push_str(&home.to_string_lossy());
        ""
    } else if let Some(r) = input.strip_prefix("~/") {
        s.push_str(&home.to_string_lossy());
        s.push('/');
        r
    } else {
        input
    };
    let chars: Vec<char> = rest.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() {
            let (name, next) = if chars[i + 1] == '{' {
                match chars[i + 2..].iter().position(|&c| c == '}') {
                    Some(end) => (chars[i + 2..i + 2 + end].iter().collect::<String>(), i + 3 + end),
                    None => (String::new(), i),
                }
            } else {
                let len = chars[i + 1..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == '_').count();
                (chars[i + 1..i + 1 + len].iter().collect::<String>(), i + 1 + len)
            };
            if !name.is_empty() {
                s.push_str(&get(&name).unwrap_or_default());
                i = next;
                continue;
            }
        }
        s.push(chars[i]);
        i += 1;
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn defaults_follow_xdg_under_home() {
        let p = Paths::resolve(env_of(&[]), PathBuf::from("/home/j"));
        assert_eq!(p.config_dir, PathBuf::from("/home/j/.config/lmdeck"));
        assert_eq!(p.cache_dir, PathBuf::from("/home/j/.cache/lmdeck"));
        assert_eq!(p.state_dir, PathBuf::from("/home/j/.local/state/lmdeck"));
        assert_eq!(p.config_file(), PathBuf::from("/home/j/.config/lmdeck/config.toml"));
    }

    #[test]
    fn xdg_overrides_and_lmdeck_home() {
        let p = Paths::resolve(env_of(&[("XDG_CONFIG_HOME", "/x/c"), ("XDG_CACHE_HOME", "")]), "/h".into());
        assert_eq!(p.config_dir, PathBuf::from("/x/c/lmdeck"));
        assert_eq!(p.cache_dir, PathBuf::from("/h/.cache/lmdeck"));
        let p = Paths::resolve(env_of(&[("LMDECK_HOME", "/t")]), "/h".into());
        assert_eq!(p.config_dir, PathBuf::from("/t/config"));
        assert_eq!(p.run_dir(), PathBuf::from("/t/state/run"));
    }

    #[test]
    fn expands_tilde_and_vars() {
        let get = env_of(&[("FOO", "bar")]);
        let home = Path::new("/home/j");
        assert_eq!(expand_with("~/models", &get, home), PathBuf::from("/home/j/models"));
        assert_eq!(expand_with("~", &get, home), PathBuf::from("/home/j"));
        assert_eq!(expand_with("/a/$FOO/${FOO}x", &get, home), PathBuf::from("/a/bar/barx"));
        assert_eq!(expand_with("/a/$NOPE/b", &get, home), PathBuf::from("/a//b"));
        assert_eq!(expand_with("cost$", &get, home), PathBuf::from("cost$"));
    }
}
