//! `lmdeck add <repo>`: turn a repo that ships `start.sh` / `stop.sh` into a
//! wrapper recipe (engine `custom`, `[source]` checkout, `detached`).
//!
//! Settings are read from simple `VAR="${VAR:-default}"` lines in the repo's
//! shell scripts (`PORT`, `HOST`, `CONTAINER_NAME`, …) so the recipe can pass
//! lmdeck's values back in through the environment. Anything it cannot work
//! out is left as a `TODO` comment in the written recipe.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths::expand;

/// What `lmdeck add` found in a checkout.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Detected {
    /// `NAME → default` from `NAME="${NAME:-default}"` lines (first one wins).
    pub defaults: BTreeMap<String, String>,
    pub port_var: Option<String>,
    pub port: Option<u16>,
    pub host_var: Option<String>,
    pub container_var: Option<String>,
    pub container: Option<String>,
    /// start.sh hands the server off and exits (`docker run -d`, `nohup`, `&`, …).
    pub detached: bool,
    pub uses_docker: bool,
}

/// `NAME="${NAME:-default}"` (also `${NAME-default}`, `${NAME:=default}`,
/// single quotes, no quotes, a leading `export`). Defaults that themselves
/// expand variables are skipped.
pub fn parse_defaults(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let mut l = line.trim();
        for kw in ["export ", "readonly ", "declare -x ", "local "] {
            if let Some(rest) = l.strip_prefix(kw) {
                l = rest.trim_start();
            }
        }
        let Some((name, value)) = l.split_once('=') else { continue };
        let valid = !name.is_empty()
            && name.chars().next().is_some_and(|c| c.is_ascii_uppercase() || c == '_')
            && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !valid {
            continue;
        }
        // Up to the first unquoted space (drop trailing `# comments`).
        let value = value.trim();
        let value = match value.chars().next() {
            Some(q @ ('"' | '\'')) => match value[1..].find(q) {
                Some(end) => &value[1..1 + end],
                None => continue,
            },
            _ => value.split_whitespace().next().unwrap_or(""),
        };
        let Some(inner) = value.strip_prefix("${").and_then(|v| v.strip_suffix('}')) else { continue };
        let Some(rest) = inner.strip_prefix(name) else { continue };
        let Some(default) = [":-", ":=", "-", "="].iter().find_map(|op| rest.strip_prefix(op)) else { continue };
        if default.contains(['$', '`']) {
            continue;
        }
        out.entry(name.to_string()).or_insert_with(|| default.to_string());
    }
    out
}

fn docker_name_ok(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Shell files worth reading: the root `*.sh`, `.env.example`, and `scripts/*.sh`.
fn script_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = vec![];
    for d in [dir.to_path_buf(), dir.join("scripts")] {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        let mut v: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && (p.extension().is_some_and(|e| e == "sh")
                        || p.file_name().is_some_and(|n| n == ".env.example" || n == "env.example"))
            })
            .collect();
        v.sort();
        files.extend(v);
    }
    files
}

/// Looks at a checkout. Fails unless it has `start.sh` and `stop.sh` at the root.
pub fn detect(dir: &Path) -> Result<Detected> {
    let missing: Vec<&str> = ["start.sh", "stop.sh"].into_iter().filter(|f| !dir.join(f).is_file()).collect();
    if !missing.is_empty() {
        bail!(
            "{} has no {} at its root; `lmdeck add` wraps repos that ship start.sh and stop.sh (write a recipe by hand for others)",
            dir.display(),
            missing.join(" or ")
        );
    }
    let mut d = Detected::default();
    let start = fs::read_to_string(dir.join("start.sh")).unwrap_or_default();
    let mut all = String::new();
    for f in script_files(dir) {
        let text = fs::read_to_string(&f).unwrap_or_default();
        for (k, v) in parse_defaults(&text) {
            d.defaults.entry(k).or_insert(v);
        }
        all.push_str(&text);
        all.push('\n');
    }
    let numeric = |k: &str| d.defaults.get(k).and_then(|v| v.parse::<u16>().ok()).filter(|p| *p > 0);
    let port_var = ["PORT", "SERVER_PORT", "API_PORT", "HTTP_PORT", "LISTEN_PORT"]
        .iter()
        .map(|s| s.to_string())
        .find(|k| numeric(k).is_some())
        .or_else(|| d.defaults.keys().find(|k| k.ends_with("_PORT") && numeric(k).is_some()).cloned());
    d.port = port_var.as_deref().and_then(numeric);
    d.port_var = port_var;
    d.host_var = ["HOST", "SERVER_HOST", "BIND_HOST", "LISTEN_HOST"]
        .iter()
        .map(|s| s.to_string())
        .find(|k| d.defaults.contains_key(k));
    let named = |k: &str| d.defaults.get(k).filter(|v| docker_name_ok(v)).is_some();
    let container_var = ["CONTAINER_NAME", "CONTAINER"]
        .iter()
        .map(|s| s.to_string())
        .find(|k| named(k))
        .or_else(|| d.defaults.keys().find(|k| k.contains("CONTAINER") && named(k)).cloned());
    d.container = container_var.as_deref().and_then(|k| d.defaults.get(k).cloned());
    d.container_var = container_var;
    d.uses_docker = all.contains("docker run") || all.contains("docker compose") || all.contains("docker-compose");
    let s = start.as_str();
    d.detached = (s.contains("docker run") && (s.contains(" -d ") || s.contains(" -d\\") || s.contains("--detach")))
        || s.contains("compose up -d")
        || s.contains("nohup ")
        || s.contains("setsid ")
        || (d.uses_docker && s.contains("docker run -d"));
    // A start.sh that defers to other scripts: look for the same markers there.
    if !d.detached && d.uses_docker {
        d.detached = all.contains("docker run -d") || all.contains("--detach") || all.contains("compose up -d");
    }
    Ok(d)
}

/// `github:o/r`, `o/r`, `https://github.com/o/r[.git][/tree/<ref>]`, any git
/// URL, or a local directory → (clone URL, ref from the URL, default name).
pub fn parse_repo_arg(arg: &str) -> Result<(String, Option<String>, String)> {
    let a = arg.trim().trim_end_matches('/');
    let local = expand(a);
    if local.is_dir() {
        let abs = fs::canonicalize(&local).unwrap_or(local);
        let name = abs.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "wrapped".into());
        return Ok((abs.display().to_string(), None, sanitize(&name)));
    }
    let gh = |path: &str| -> Result<(String, Option<String>, String)> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.len() < 2 {
            bail!("'{arg}' is not a GitHub repo (expected owner/repo)");
        }
        let repo = parts[1].trim_end_matches(".git");
        let git_ref = (parts.len() > 3 && parts[2] == "tree").then(|| parts[3..].join("/"));
        Ok((format!("https://github.com/{}/{repo}", parts[0]), git_ref, sanitize(repo)))
    };
    if let Some(rest) = a.strip_prefix("github:") {
        return gh(rest);
    }
    for prefix in ["https://github.com/", "http://github.com/", "github.com/"] {
        if let Some(rest) = a.strip_prefix(prefix) {
            return gh(rest);
        }
    }
    if a.contains("://") || a.starts_with("git@") {
        let last = a.rsplit(['/', ':']).next().unwrap_or("wrapped").trim_end_matches(".git");
        return Ok((a.to_string(), None, sanitize(last)));
    }
    let parts: Vec<&str> = a.split('/').collect();
    if parts.len() == 2 && parts.iter().all(|p| !p.is_empty()) {
        return gh(a);
    }
    bail!("'{arg}' is not a directory, a GitHub owner/repo, or a git URL")
}

/// Lower-cased, `[a-z0-9._-]` only.
pub fn sanitize(s: &str) -> String {
    let out: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    let out = out.trim_matches(|c| c == '-' || c == '.').to_string();
    if out.is_empty() {
        "wrapped".into()
    } else {
        out
    }
}

fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// What to write into the recipe's `[source]`.
pub struct SourcePin<'a> {
    pub repo: &'a str,
    /// The ref to record (a commit, or the branch/tag the user asked for).
    pub git_ref: &'a str,
    /// For the comment: the branch the commit came from.
    pub branch: Option<&'a str>,
}

/// The wrapper recipe text (TOML with comments; `TODO`s where detection failed).
pub fn recipe_toml(name: &str, display: &str, pin: &SourcePin, d: &Detected) -> String {
    let mut t = String::new();
    t.push_str(&format!("# Wrapper for {}, written by `lmdeck add`.\n", pin.repo));
    t.push_str("# lmdeck runs the repo's own start.sh / stop.sh from its checkout; it applies no patches\n");
    t.push_str("# and builds nothing itself.\n");
    t.push_str(&format!("name = {}\n", q(name)));
    t.push_str(&format!("display_name = {}\n", q(display)));
    t.push_str(&format!("description = {}\n", q(&format!("Runs {display}'s start.sh / stop.sh."))));
    t.push_str("tags = [\"wrapped\"]\n\n[source]\n");
    t.push_str(&format!("repo = {}\n", q(pin.repo)));
    if let Some(b) = pin.branch.filter(|b| *b != pin.git_ref) {
        t.push_str(&format!(
            "# The tip of '{b}' when added. Use ref = {} to follow the branch on `lmdeck pull`.\n",
            q(b)
        ));
    }
    t.push_str(&format!("ref = {}\n\n[engine]\nname = \"custom\"\n", q(pin.git_ref)));
    t.push_str("command = [\"./start.sh\"]\nstop = [\"./stop.sh\"]\n");
    if d.detached {
        t.push_str("# start.sh hands the server off and exits once it is up.\ndetached = true\n");
    } else {
        t.push_str("# TODO: if start.sh exits once the server is up (it backgrounds it or runs `docker run -d`),\n");
        t.push_str("# set detached = true (and container / logs below).\ndetached = false\n");
    }
    match d.port {
        Some(p) => t.push_str(&format!("port = {p}\n")),
        None => {
            t.push_str("# TODO: the port start.sh serves on (no PORT=${PORT:-…} default found).\nport = 8080\n");
        }
    }
    match (&d.container, d.detached) {
        (Some(c), true) => {
            t.push_str(&format!("container = {}\n", q(c)));
            t.push_str(&format!("logs = [\"docker\", \"logs\", \"-f\", {}]\n", q(c)));
        }
        (Some(c), false) => t.push_str(&format!("container = {}\n", q(c))),
        (None, true) if d.uses_docker => {
            t.push_str("# TODO: the Docker container start.sh runs, for status, `docker stop` and logs:\n");
            t.push_str("# container = \"<name>\"\n# logs = [\"docker\", \"logs\", \"-f\", \"<name>\"]\n");
        }
        (None, _) => {}
    }
    t.push_str("# First runs often build images or download weights before the server answers.\n");
    t.push_str("ready_timeout = 3600\n");
    let mut env: Vec<(String, &str)> = vec![];
    if let Some(v) = &d.port_var {
        env.push((v.clone(), "{port}"));
    }
    if let Some(v) = &d.host_var {
        env.push((v.clone(), "{host}"));
    }
    if let Some(v) = d.container_var.as_ref().filter(|_| d.container.is_some()) {
        env.push((v.clone(), "{container}"));
    }
    t.push_str("\n# lmdeck's settings, handed to the scripts as environment variables.\n[engine.env]\n");
    if env.is_empty() {
        t.push_str("# TODO: e.g. PORT = \"{port}\" if start.sh reads its port from the environment.\n");
    }
    for (k, v) in env {
        t.push_str(&format!("{k} = {}\n", q(v)));
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::Recipe;

    #[test]
    fn parses_shell_defaults() {
        let text = r#"
PORT="${PORT:-8888}"   # the API port
export HOST=${HOST:-0.0.0.0}
CONTAINER_NAME='${CONTAINER_NAME:-ornith-122b-tf}'
IMAGE="${IMAGE:-tensorfold-ornith:${TF_TAG}}"
EMPTY="${EMPTY:-}"
OTHER="${NOT_OTHER:-1}"
lower="${lower:-x}"
PORT="${PORT:-9999}"
DASH=${DASH-7}
"#;
        let d = parse_defaults(text);
        assert_eq!(d["PORT"], "8888", "first wins");
        assert_eq!(d["HOST"], "0.0.0.0");
        assert_eq!(d["CONTAINER_NAME"], "ornith-122b-tf");
        assert_eq!(d["EMPTY"], "");
        assert_eq!(d["DASH"], "7");
        assert!(!d.contains_key("IMAGE") && !d.contains_key("OTHER") && !d.contains_key("lower"));
    }

    #[test]
    fn repo_args() {
        let (u, r, n) = parse_repo_arg("https://github.com/MiaAI-Lab/Ornith-1.5-122B-TensorFold").unwrap();
        assert_eq!(
            (u.as_str(), r, n.as_str()),
            ("https://github.com/MiaAI-Lab/Ornith-1.5-122B-TensorFold", None, "ornith-1.5-122b-tensorfold")
        );
        let (u, r, _) = parse_repo_arg("github:o/r.git").unwrap();
        assert_eq!((u.as_str(), r), ("https://github.com/o/r", None));
        let (_, r, _) = parse_repo_arg("https://github.com/o/r/tree/dev").unwrap();
        assert_eq!(r.as_deref(), Some("dev"));
        let (u, _, n) = parse_repo_arg("git@gitlab.com:me/My_Server.git").unwrap();
        assert_eq!((u.as_str(), n.as_str()), ("git@gitlab.com:me/My_Server.git", "my_server"));
        assert_eq!(parse_repo_arg("o/r").unwrap().0, "https://github.com/o/r");
        assert!(parse_repo_arg("nope").is_err());
    }

    fn checkout(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (f, body) in files {
            let p = d.path().join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        d
    }

    #[test]
    fn detects_mia_style_repo() {
        let d = checkout(&[
            (
                "start.sh",
                "#!/usr/bin/env bash\nsource ./scripts/config.sh\ndocker run -d --name \"$CONTAINER_NAME\" img\n",
            ),
            ("stop.sh", "docker stop \"$CONTAINER_NAME\"\n"),
            (
                "scripts/config.sh",
                "CONTAINER_NAME=\"${CONTAINER_NAME:-srv-1}\"\nHOST=\"${HOST:-0.0.0.0}\"\nPORT=\"${PORT:-8888}\"\n",
            ),
        ]);
        let det = detect(d.path()).unwrap();
        assert_eq!(det.port, Some(8888));
        assert_eq!(det.container.as_deref(), Some("srv-1"));
        assert!(det.detached && det.uses_docker);
        let pin = SourcePin { repo: "https://github.com/x/srv", git_ref: "abc123", branch: Some("main") };
        let text = recipe_toml("srv", "srv", &pin, &det);
        assert!(!text.contains("TODO"), "{text}");
        let r = Recipe::parse(&text, "srv").unwrap();
        assert!(r.engine.detached);
        assert_eq!(r.port(), 8888);
        assert_eq!(r.engine.logs, ["docker", "logs", "-f", "srv-1"]);
        assert_eq!(r.engine.env["PORT"], "{port}");
        assert_eq!(r.engine.env["CONTAINER_NAME"], "{container}");
        assert_eq!(r.source.unwrap().git_ref.as_deref(), Some("abc123"));
    }

    #[test]
    fn leaves_todos_when_unsure() {
        let d = checkout(&[("start.sh", "exec ./server --port 9000\n"), ("stop.sh", "pkill server\n")]);
        let det = detect(d.path()).unwrap();
        assert_eq!((det.port, det.detached, det.container.clone()), (None, false, None));
        let pin = SourcePin { repo: "/tmp/x", git_ref: "main", branch: Some("main") };
        let text = recipe_toml("x", "x", &pin, &det);
        assert!(text.matches("TODO").count() >= 3, "{text}");
        assert!(Recipe::parse(&text, "x").is_ok(), "{text}");

        let d = checkout(&[("start.sh", "x\n")]);
        assert!(detect(d.path()).unwrap_err().to_string().contains("stop.sh"));
    }
}
