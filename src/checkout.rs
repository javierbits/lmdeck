//! `[source]` git checkouts for wrapped (`custom`) recipes.
//!
//! lmdeck clones `source.repo` into `<cache>/checkouts/<recipe>` and checks
//! out `source.ref` (a branch follows `origin/<branch>`; a tag or commit is
//! used as is). It never edits the checkout: no patches, no builds. Local
//! changes to tracked files make an update fail rather than be discarded;
//! untracked files such as a `.env` are left alone.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::engine::checkout_dir;
use crate::paths::Paths;
use crate::recipe::{GitSource, Recipe};

/// What [`sync`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Freshly cloned, at this commit.
    Cloned(String),
    /// Fetched and moved from one commit to another.
    Updated { from: String, to: String },
    /// Fetched; already at the target commit.
    UpToDate(String),
    /// Already checked out; not fetched (`update = false`).
    Present(String),
}

impl Outcome {
    pub fn commit(&self) -> &str {
        match self {
            Outcome::Cloned(c) | Outcome::UpToDate(c) | Outcome::Present(c) => c,
            Outcome::Updated { to, .. } => to,
        }
    }

    pub fn describe(&self) -> String {
        let short = |c: &str| c.chars().take(10).collect::<String>();
        match self {
            Outcome::Cloned(c) => format!("cloned at {}", short(c)),
            Outcome::Updated { from, to } => format!("updated {} → {}", short(from), short(to)),
            Outcome::UpToDate(c) => format!("up to date at {}", short(c)),
            Outcome::Present(c) => format!("checked out at {}", short(c)),
        }
    }
}

fn git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut c = Command::new("git");
    if let Some(d) = dir {
        c.arg("-C").arg(d);
    }
    c.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ADVICE", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = c.output().context("running git (is it installed?)")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The commit-ish to check out for `git_ref` in `dir`.
fn target(dir: &Path, git_ref: Option<&str>) -> Result<String> {
    let r = git_ref.map(str::trim).filter(|r| !r.is_empty());
    let Some(r) = r else { return Ok("origin/HEAD".into()) };
    if git(Some(dir), &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/origin/{r}^{{commit}}")]).is_ok() {
        return Ok(format!("origin/{r}"));
    }
    git(Some(dir), &["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")])
        .map_err(|_| anyhow!("ref '{r}' not found in the repository (not a branch, tag or fetched commit)"))?;
    Ok(r.to_string())
}

fn checkout(dir: &Path, git_ref: Option<&str>) -> Result<String> {
    let t = target(dir, git_ref)?;
    git(Some(dir), &["checkout", "--quiet", "--detach", &t])?;
    git(Some(dir), &["rev-parse", "HEAD"])
}

/// Whether the checkout in `dir` is at `head`, the commit `git_ref` names
/// there as fetched so far (`false` when it names none yet).
fn at_ref(dir: &Path, git_ref: Option<&str>, head: &str) -> bool {
    let want = target(dir, git_ref).and_then(|t| git(Some(dir), &["rev-parse", &format!("{t}^{{commit}}")]));
    want.is_ok_and(|c| c == head)
}

/// Clones `src` into `dest` if it is not there yet. Otherwise it fetches and
/// checks out `source.ref` again when `update` is set, or when the checkout
/// isn't at the ref (the recipe's ref changed, or names a commit not fetched yet).
pub fn sync(src: &GitSource, dest: &Path, update: bool) -> Result<Outcome> {
    let git_ref = src.git_ref.as_deref();
    if dest.join(".git").exists() {
        let before = git(Some(dest), &["rev-parse", "HEAD"])?;
        if !update && at_ref(dest, git_ref, &before) {
            return Ok(Outcome::Present(before));
        }
        git(Some(dest), &["fetch", "--quiet", "--tags", "--force", "origin"])?;
        let after = checkout(dest, git_ref)?;
        return Ok(if after == before {
            Outcome::UpToDate(after)
        } else {
            Outcome::Updated { from: before, to: after }
        });
    }
    if dest.exists() && fs::read_dir(dest)?.next().is_some() {
        bail!("{} exists but is not a git checkout; move it away first", dest.display());
    }
    let parent = dest.parent().ok_or_else(|| anyhow!("bad checkout path {}", dest.display()))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.clone-{}",
        dest.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    let result = (|| -> Result<String> {
        git(None, &["clone", "--quiet", &src.repo, &tmp.to_string_lossy()])?;
        checkout(&tmp, git_ref)
    })();
    match result {
        Ok(commit) => {
            let _ = fs::remove_dir(dest); // an empty leftover dir
            fs::rename(&tmp, dest).with_context(|| format!("moving the checkout to {}", dest.display()))?;
            Ok(Outcome::Cloned(commit))
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            Err(e.context(format!("cloning {}", src.repo)))
        }
    }
}

/// Makes sure a recipe's `[source]` is checked out at its ref (cloning it the
/// first time; fetching only when the checkout isn't at the ref). `None` for
/// recipes without a `[source]`.
pub fn ensure(paths: &Paths, r: &Recipe) -> Result<Option<Outcome>> {
    match &r.source {
        Some(src) => sync(src, &checkout_dir(&paths.checkouts_dir(), &r.name), false).map(Some),
        None => Ok(None),
    }
}

/// Fetches a recipe's `[source]` and checks out its ref again (`lmdeck pull`).
pub fn update(paths: &Paths, r: &Recipe) -> Result<Option<Outcome>> {
    match &r.source {
        Some(src) => sync(src, &checkout_dir(&paths.checkouts_dir(), &r.name), true).map(Some),
        None => Ok(None),
    }
}

/// `origin`'s URL for an existing checkout.
pub fn origin_url(dir: &Path) -> Option<String> {
    git(Some(dir), &["remote", "get-url", "origin"]).ok()
}

/// The remote's default branch (`main`), from `origin/HEAD`.
pub fn default_branch(dir: &Path) -> Option<String> {
    let r = git(Some(dir), &["rev-parse", "--abbrev-ref", "origin/HEAD"]).ok()?;
    Some(r.strip_prefix("origin/").unwrap_or(&r).to_string()).filter(|b| !b.is_empty() && b != "HEAD")
}

/// The commands [`sync`] would run, for `--dry-run`.
pub fn describe_sync(src: &GitSource, dest: &Path, update: bool) -> Vec<String> {
    let r = src.git_ref.as_deref().filter(|r| !r.is_empty()).unwrap_or("<default branch>");
    if dest.join(".git").exists() {
        let head = git(Some(dest), &["rev-parse", "HEAD"]).unwrap_or_default();
        if update || !at_ref(dest, src.git_ref.as_deref(), &head) {
            vec![format!("git -C {} fetch origin && git checkout --detach {r}", dest.display())]
        } else {
            vec![]
        }
    } else {
        vec![format!("git clone {} {} && git checkout --detach {r}", src.repo, dest.display())]
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    pub(crate) fn sh_git(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false", "-C"])
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    /// A local repo with one commit on `main`.
    pub(crate) fn fixture_repo(root: &Path, files: &[(&str, &str)]) -> PathBuf {
        let repo = root.join("upstream");
        fs::create_dir_all(&repo).unwrap();
        sh_git(&repo, &["init", "--quiet", "-b", "main"]);
        for (f, body) in files {
            let p = repo.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        sh_git(&repo, &["add", "-A"]);
        sh_git(&repo, &["commit", "--quiet", "-m", "one"]);
        repo
    }

    #[test]
    fn clones_then_updates_and_pins() {
        let d = tempfile::tempdir().unwrap();
        let repo = fixture_repo(d.path(), &[("start.sh", "echo v1\n")]);
        let src = GitSource { repo: repo.display().to_string(), git_ref: Some("main".into()), branch: None };
        let dest = d.path().join("cache/checkouts/r");
        assert_eq!(describe_sync(&src, &dest, false).len(), 1);

        let first = sync(&src, &dest, false).unwrap();
        assert!(matches!(first, Outcome::Cloned(_)), "{first:?}");
        assert_eq!(fs::read_to_string(dest.join("start.sh")).unwrap(), "echo v1\n");
        assert!(matches!(sync(&src, &dest, false).unwrap(), Outcome::Present(_)));
        assert!(matches!(sync(&src, &dest, true).unwrap(), Outcome::UpToDate(_)));

        // A new upstream commit: an update moves the branch checkout forward;
        // an untracked .env survives.
        fs::write(dest.join(".env"), "PORT=1\n").unwrap();
        fs::write(repo.join("start.sh"), "echo v2\n").unwrap();
        sh_git(&repo, &["commit", "--quiet", "-am", "two"]);
        let up = sync(&src, &dest, true).unwrap();
        assert!(matches!(&up, Outcome::Updated { from, to } if from != to), "{up:?}");
        assert_eq!(fs::read_to_string(dest.join("start.sh")).unwrap(), "echo v2\n");
        assert!(dest.join(".env").exists());

        // Pinning to the first commit.
        let pinned = GitSource { git_ref: Some(first.commit().to_string()), ..src.clone() };
        let back = sync(&pinned, &dest, true).unwrap();
        assert_eq!(back.commit(), first.commit());
        assert_eq!(fs::read_to_string(dest.join("start.sh")).unwrap(), "echo v1\n");

        // Bad refs and bad repos fail cleanly, leaving nothing behind.
        let bad = GitSource { git_ref: Some("no-such-branch".into()), ..src.clone() };
        assert!(sync(&bad, &d.path().join("cache/checkouts/bad"), false).is_err());
        assert!(!d.path().join("cache/checkouts/bad").exists());
        let missing = GitSource { repo: d.path().join("nope").display().to_string(), git_ref: None, branch: None };
        assert!(sync(&missing, &d.path().join("cache/checkouts/m"), false).is_err());
    }
}
