//! `schema = N` on recipes, the index and the config.
//!
//! Every file lmdeck reads or writes carries a top-level `schema` integer.
//! A missing `schema` means 1 (every file written before the key existed).
//! A file from a newer lmdeck (`schema` above [`CURRENT`]) is not guessed at:
//! it is skipped with a warning that says which lmdeck it needs. The file may
//! say so itself with `min_lmdeck = "x.y.z"`. Older schemas go through
//! [`migrate`], one step at a time, before they are deserialised.

use std::fmt;

/// The schema this lmdeck reads and writes.
pub const CURRENT: u32 = 1;

/// Marker that starts every unsupported-schema message (doctor groups by it).
pub const MARKER: &str = "unsupported schema";

/// What a file declared, when lmdeck can't read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    /// `recipe`, `index` or `config`.
    pub kind: &'static str,
    pub found: u32,
    /// The file's own `min_lmdeck`, if it gave one.
    pub min_lmdeck: Option<String>,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let needs = match &self.min_lmdeck {
            Some(v) => format!("needs lmdeck >= {v} / schema {}", self.found),
            None => format!("needs lmdeck newer than {} / schema {}", env!("CARGO_PKG_VERSION"), self.found),
        };
        write!(
            f,
            "{MARKER}: this {} is schema {}, lmdeck {} reads schema {CURRENT}; {needs} (skipped)",
            self.kind,
            self.found,
            env!("CARGO_PKG_VERSION")
        )
    }
}

impl std::error::Error for Unsupported {}

/// The declared schema of a parsed TOML document (missing = 1).
pub fn declared(v: &toml::Value) -> Result<u32, String> {
    match v.get("schema") {
        None => Ok(1),
        Some(toml::Value::Integer(n)) if *n >= 1 && *n <= u32::MAX as i64 => Ok(*n as u32),
        Some(other) => Err(format!("`schema` must be a positive integer, got {other}")),
    }
}

/// Checks `v`'s schema and brings it up to [`CURRENT`]. A newer schema is an
/// [`Unsupported`] error (wrapped in anyhow so callers can downcast it).
pub fn check(kind: &'static str, v: toml::Value) -> anyhow::Result<toml::Value> {
    let found = declared(&v).map_err(anyhow::Error::msg)?;
    if found > CURRENT {
        let min_lmdeck = v.get("min_lmdeck").and_then(|m| m.as_str()).map(str::to_string);
        return Err(Unsupported { kind, found, min_lmdeck }.into());
    }
    Ok(migrate(kind, v, found))
}

/// The migration hook: rewrites a schema-`from` document into the current
/// schema, one version at a time. Schema 1 is current, so nothing changes
/// yet; a schema 2 adds an arm `1 => …` here (and bumps [`CURRENT`]).
pub fn migrate(kind: &'static str, mut v: toml::Value, from: u32) -> toml::Value {
    for at in from..CURRENT {
        v = step(kind, at, v);
    }
    if let Some(t) = v.as_table_mut() {
        t.insert("schema".into(), toml::Value::Integer(CURRENT as i64));
    }
    v
}

/// One migration step, `from` → `from + 1`. Schema 2 goes here as
/// `match (kind, from) { ("recipe", 1) => …, _ => v }`.
fn step(_kind: &'static str, _from: u32, v: toml::Value) -> toml::Value {
    v
}

/// Is this error (or anything in its chain) an unsupported schema?
pub fn is_unsupported(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<Unsupported>().is_some())
}

/// serde default for `schema` fields.
pub fn current() -> u32 {
    CURRENT
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> toml::Value {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn missing_means_one_and_current_passes() {
        assert_eq!(declared(&v("a = 1")).unwrap(), 1);
        let out = check("recipe", v("a = 1")).unwrap();
        assert_eq!(out.get("schema").and_then(|s| s.as_integer()), Some(CURRENT as i64));
        assert!(check("recipe", v("schema = 1\na = 1")).is_ok());
    }

    #[test]
    fn newer_is_refused_with_what_it_needs() {
        let e = check("index", v("schema = 7\nmin_lmdeck = \"0.9.0\"")).unwrap_err();
        assert!(is_unsupported(&e));
        let msg = format!("{e}");
        assert!(msg.starts_with(MARKER), "{msg}");
        assert!(msg.contains("needs lmdeck >= 0.9.0 / schema 7"), "{msg}");
        let e = check("recipe", v("schema = 2")).unwrap_err();
        assert!(format!("{e}").contains("needs lmdeck newer than"), "{e}");
        assert!(format!("{e}").contains("/ schema 2"), "{e}");
    }

    #[test]
    fn junk_schema_is_an_error_not_a_skip() {
        let e = check("config", v("schema = \"one\"")).unwrap_err();
        assert!(!is_unsupported(&e));
        assert!(check("config", v("schema = 0")).is_err());
    }
}
