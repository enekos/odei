//! Worktree tool: find a checkout, open one, work in it, retire it once merged.
//!
//! Directory resolution goes through `zz` (github.com/enekos/zz) when it is on
//! PATH — zoxide frecency plus its own worktree discovery, so a bare branch
//! fragment lands on a checkout the agent has never visited. Where git can
//! answer for itself it is asked directly, so every action still works on a
//! machine without `zz`.

use super::{terminal, ToolContext, ToolOutcome};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn worktree(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    let action = input["action"].as_str().unwrap_or_default();
    let result = match action {
        "resolve" => resolve(ctx, input),
        "list" => list(ctx, input),
        "create" => create(ctx, input, Missing::Branch),
        "track" => create(ctx, input, Missing::Remote),
        "run" => return run(ctx, input),
        "merged" => survey(ctx, input, false),
        "clean" => survey(ctx, input, true),
        "" => Err("worktree requires an action".to_string()),
        other => Err(format!("unknown action {other}")),
    };
    match result {
        Ok(text) => ToolOutcome::ok(text),
        Err(text) => ToolOutcome::err(text),
    }
}

// ── zz ──────────────────────────────────────────────────────────────────────

/// `zz` on PATH, or where its own installer puts it.
fn zz_bin() -> Option<PathBuf> {
    crate::cli::which("zz").or_else(|| {
        let local = PathBuf::from(std::env::var_os("HOME")?).join(".local/bin/zz");
        local.is_file().then_some(local)
    })
}

fn zz(args: &[&str]) -> Result<String, String> {
    let bin = zz_bin().ok_or("zz is not installed")?;
    let out = Command::new(&bin).args(args).output().map_err(|e| format!("zz: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if stderr.is_empty() { format!("zz {}: no match", args.join(" ")) } else { stderr })
    }
}

// ── git ─────────────────────────────────────────────────────────────────────

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if stderr.is_empty() { format!("git {} failed", args.join(" ")) } else { stderr })
    }
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git(dir, args).is_ok()
}

struct Worktree {
    path: PathBuf,
    branch: Option<String>,
    main: bool,
}

impl Worktree {
    fn label(&self) -> &str {
        self.branch.as_deref().unwrap_or("(detached)")
    }
}

/// The repository owning `dir`, following a linked worktree back to its main
/// checkout via the shared git dir.
fn repo_root(dir: &Path) -> Result<PathBuf, String> {
    let common = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    PathBuf::from(&common)
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("{common} has no parent"))
}

fn worktrees(repo: &Path) -> Result<Vec<Worktree>, String> {
    let text = git(repo, &["worktree", "list", "--porcelain"])?;
    let mut found: Vec<Worktree> = Vec::new();
    for block in text.split("\n\n") {
        let mut path = None;
        let mut branch = None;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(rest));
            } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
                branch = Some(rest.to_string());
            }
        }
        if let Some(path) = path {
            let main = found.is_empty();
            found.push(Worktree { path, branch, main });
        }
    }
    Ok(found)
}

/// The branch a merge is measured against: origin's default, else a local one.
fn base_ref(repo: &Path) -> String {
    if let Ok(head) = git(repo, &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"]) {
        if let Some(name) = head.strip_prefix("refs/remotes/") {
            return name.to_string();
        }
    }
    for candidate in ["origin/main", "origin/master", "main", "master"] {
        if git_ok(repo, &["rev-parse", "--verify", "--quiet", candidate]) {
            return candidate.to_string();
        }
    }
    "HEAD".to_string()
}

// ── resolution ──────────────────────────────────────────────────────────────

/// `data@fix-foo` and `{query: "data", branch: "fix-foo"}` mean the same thing.
fn split_query(input: &Value) -> (String, Option<String>) {
    let raw = input["query"].as_str().unwrap_or_default().trim();
    let explicit = input["branch"].as_str().map(str::trim).filter(|b| !b.is_empty());
    match raw.split_once('@') {
        Some((query, branch)) if explicit.is_none() && !branch.is_empty() => {
            (query.to_string(), Some(branch.to_string()))
        }
        _ => (raw.trim_end_matches('@').to_string(), explicit.map(str::to_string)),
    }
}

/// Most-specific-first branch matching, mirroring zz: exact name, then unique
/// prefix, then unique substring. Ambiguity is an error, never a guess.
fn match_branch<'a>(trees: &'a [Worktree], fragment: &str) -> Result<&'a Worktree, String> {
    let named: Vec<&Worktree> = trees.iter().filter(|w| w.branch.is_some()).collect();
    if let Some(exact) = named.iter().find(|w| w.label() == fragment) {
        return Ok(exact);
    }
    for tier in [
        |name: &str, f: &str| name.starts_with(f),
        |name: &str, f: &str| name.contains(f),
    ] {
        let hits: Vec<&&Worktree> = named.iter().filter(|w| tier(w.label(), fragment)).collect();
        match hits.as_slice() {
            [only] => return Ok(only),
            [] => continue,
            many => {
                let names: Vec<&str> = many.iter().map(|w| w.label()).collect();
                return Err(format!("{fragment} is ambiguous: {}", names.join(", ")));
            }
        }
    }
    Err(format!("no worktree matches {fragment}"))
}

enum Missing {
    /// Cut or check out a local branch (`zz -c`).
    Branch,
    /// Fetch and track a remote branch (`zz -t`).
    Remote,
}

/// Resolve a query to a directory. `create` decides what happens when the
/// requested worktree does not exist yet; without it, a miss is an error.
fn locate(ctx: &ToolContext, input: &Value, create: Option<Missing>) -> Result<PathBuf, String> {
    let (query, branch) = split_query(input);
    let root = input["root"].as_bool().unwrap_or(false);

    // An empty query means this workspace, which needs no lookup at all.
    if query.is_empty() {
        return from_repo(ctx.workspace_root.clone(), branch, root, create);
    }
    if zz_bin().is_some() {
        let target = match &branch {
            Some(branch) => format!("{query}@{branch}"),
            None => query.clone(),
        };
        let mut args = vec!["-p"];
        if root {
            args.push("-r");
        }
        match create {
            Some(Missing::Branch) => args.push("-c"),
            Some(Missing::Remote) => args.push("-t"),
            None => {}
        }
        args.push(&target);
        return zz(&args).map(PathBuf::from);
    }
    let path = ctx.resolve(&query);
    if !path.is_dir() {
        return Err(format!(
            "zz is not installed, so {query} has to be a path — {} is not a directory",
            path.display()
        ));
    }
    from_repo(path, branch, root, create)
}

/// Resolution without zz: git's own view of the repository holding `base`.
fn from_repo(
    base: PathBuf,
    branch: Option<String>,
    root: bool,
    create: Option<Missing>,
) -> Result<PathBuf, String> {
    let repo = repo_root(&base)?;
    let Some(branch) = branch else {
        return Ok(if root { repo } else { base });
    };
    match match_branch(&worktrees(&repo)?, &branch) {
        Ok(found) => Ok(found.path.clone()),
        Err(miss) => match create {
            Some(kind) => add_worktree(&repo, &branch, kind),
            None => Err(miss),
        },
    }
}

/// zz's layout, so a worktree created here and one created by zz land together.
fn worktree_path(repo: &Path, branch: &str) -> PathBuf {
    let name = repo.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or("repo".into());
    let slug = branch.replace('/', "-");
    match std::env::var("ZZ_WORKTREE_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join(name).join(slug),
        _ => repo.parent().unwrap_or(repo).join(format!("{name}-worktrees")).join(slug),
    }
}

fn add_worktree(repo: &Path, branch: &str, kind: Missing) -> Result<PathBuf, String> {
    let path = worktree_path(repo, branch);
    let target = path.to_string_lossy().to_string();
    if git_ok(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]) {
        git(repo, &["worktree", "add", &target, branch])?;
        return Ok(path);
    }
    match kind {
        Missing::Branch => {
            let base = std::env::var("ZZ_WORKTREE_BASE").ok().filter(|b| !b.is_empty());
            let base = base.unwrap_or_else(|| {
                ["main", "master"]
                    .into_iter()
                    .find(|c| git_ok(repo, &["rev-parse", "--verify", "--quiet", c]))
                    .unwrap_or("HEAD")
                    .to_string()
            });
            git(repo, &["worktree", "add", "-b", branch, &target, &base])?;
        }
        Missing::Remote => {
            let remote = match git_ok(repo, &["remote", "get-url", "origin"]) {
                true => "origin".to_string(),
                false => git(repo, &["remote"])?
                    .lines()
                    .next()
                    .ok_or("the repository has no remotes")?
                    .to_string(),
            };
            git(repo, &["fetch", &remote, branch])?;
            let upstream = format!("{remote}/{branch}");
            git(repo, &["worktree", "add", "--track", "-b", branch, &target, &upstream])?;
        }
    }
    Ok(path)
}

// ── actions ─────────────────────────────────────────────────────────────────

fn resolve(ctx: &ToolContext, input: &Value) -> Result<String, String> {
    let dir = locate(ctx, input, None)?;
    Ok(dir.to_string_lossy().to_string())
}

fn list(ctx: &ToolContext, input: &Value) -> Result<String, String> {
    let repo = repo_root(&locate(ctx, input, None)?)?;
    let trees = worktrees(&repo)?;
    let mut lines = vec![format!("{} — {} worktree(s)", repo.display(), trees.len())];
    for tree in &trees {
        let tag = if tree.main { " (main)" } else { "" };
        lines.push(format!("{}{tag}\t{}", tree.label(), tree.path.display()));
    }
    Ok(lines.join("\n"))
}

fn create(ctx: &ToolContext, input: &Value, kind: Missing) -> Result<String, String> {
    let (_, branch) = split_query(input);
    if branch.is_none() {
        return Err("create and track need a branch, as query@branch or the branch field".into());
    }
    let dir = locate(ctx, input, Some(kind))?;
    Ok(dir.to_string_lossy().to_string())
}

fn run(ctx: &ToolContext, input: &Value) -> ToolOutcome {
    if input["command"].as_str().unwrap_or_default().trim().is_empty() {
        return ToolOutcome::err("run requires a command");
    }
    let dir = match locate(ctx, input, None) {
        Ok(dir) => dir,
        Err(text) => return ToolOutcome::err(text),
    };
    let mut exec = input.clone();
    if let Some(map) = exec.as_object_mut() {
        map.insert("action".into(), json!("exec"));
        map.insert("cwd".into(), json!(dir.to_string_lossy()));
    }
    let mut outcome = terminal::terminal(ctx, &exec);
    outcome.text = format!("in {}\n{}", dir.display(), outcome.text);
    outcome
}

// ── cleanup ─────────────────────────────────────────────────────────────────

/// Why a branch is considered done with. Squash merges leave no ancestry, so
/// GitHub's own answer counts too when `gh` is available.
enum Merged {
    Ancestor,
    Pr(u64),
    /// Sitting on the base tip: nothing landed because nothing happened.
    Fresh,
    No,
}

fn merged_pr(repo: &Path, branch: &str) -> Option<u64> {
    let out = Command::new("gh")
        .args(["pr", "list", "--head", branch, "--state", "merged", "--limit", "1"])
        .args(["--json", "number"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let parsed: Vec<Value> = serde_json::from_slice(&out.stdout).ok()?;
    parsed.first()?["number"].as_u64()
}

fn merge_status(repo: &Path, branch: &str, base: &str) -> Merged {
    let tip = git(repo, &["rev-parse", branch]).ok();
    if tip.is_some() && tip == git(repo, &["rev-parse", base]).ok() {
        return Merged::Fresh;
    }
    if git_ok(repo, &["merge-base", "--is-ancestor", branch, base]) {
        return Merged::Ancestor;
    }
    match merged_pr(repo, branch) {
        Some(number) => Merged::Pr(number),
        None => Merged::No,
    }
}

/// Report — and with `apply`, remove — worktrees whose branch has landed.
/// Skips the main checkout, whatever the session is running in, and anything
/// with uncommitted work unless `force` is set.
fn survey(ctx: &ToolContext, input: &Value, apply: bool) -> Result<String, String> {
    // A branch here narrows what gets retired; it must not steer resolution,
    // which only has to land somewhere inside the repository.
    let (query, only) = split_query(input);
    let repo = repo_root(&locate(ctx, &json!({"query": query}), None)?)?;
    let base = base_ref(&repo);
    let force = input["force"].as_bool().unwrap_or(false);
    let current = ctx.workspace_root.canonicalize().ok();

    let mut done = Vec::new();
    let mut kept = Vec::new();
    for tree in worktrees(&repo)? {
        let Some(branch) = tree.branch.clone() else {
            kept.push(format!("{}\tdetached HEAD", tree.path.display()));
            continue;
        };
        if tree.main {
            continue;
        }
        if only.as_ref().is_some_and(|b| *b != branch) {
            continue;
        }
        if tree.path.canonicalize().ok() == current {
            kept.push(format!("{branch}\tthe session is working in it"));
            continue;
        }
        let reason = match merge_status(&repo, &branch, &base) {
            Merged::Ancestor => format!("merged into {base}"),
            Merged::Pr(number) => format!("PR #{number} merged"),
            Merged::Fresh if force => "no commits of its own".to_string(),
            Merged::Fresh => {
                kept.push(format!("{branch}\tno commits of its own"));
                continue;
            }
            Merged::No => {
                kept.push(format!("{branch}\tnot merged into {base}"));
                continue;
            }
        };
        let dirty = git(&tree.path, &["status", "--porcelain"]).unwrap_or_default();
        if !dirty.is_empty() && !force {
            let count = dirty.lines().count();
            kept.push(format!("{branch}\t{reason}, but {count} uncommitted change(s)"));
            continue;
        }
        if !apply {
            done.push(format!("{branch}\t{reason}\t{}", tree.path.display()));
            continue;
        }
        let mut remove = vec!["worktree", "remove"];
        if force {
            remove.push("--force");
        }
        let target = tree.path.to_string_lossy().to_string();
        remove.push(&target);
        if let Err(why) = git(&repo, &remove) {
            kept.push(format!("{branch}\t{reason}, but removal failed: {why}"));
            continue;
        }
        let deleted = git(&repo, &["branch", "-d", &branch])
            .or_else(|_| git(&repo, &["branch", "-D", &branch]));
        let branch_note = if deleted.is_ok() { "" } else { " (branch kept)" };
        done.push(format!("{branch}\t{reason}\tremoved{branch_note}"));
    }

    if apply && !done.is_empty() {
        let _ = git(&repo, &["worktree", "prune"]);
    }

    let heading = match (apply, done.len()) {
        (true, 0) => format!("{}: nothing to clean up", repo.display()),
        (true, n) => format!("{}: retired {n} worktree(s)", repo.display()),
        (false, 0) => format!("{}: no merged worktrees", repo.display()),
        (false, n) => format!("{}: {n} merged worktree(s), against {base}", repo.display()),
    };
    let mut lines = vec![heading];
    lines.extend(done);
    if !kept.is_empty() {
        lines.push(format!("kept ({}):", kept.len()));
        lines.extend(kept);
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree(branch: &str) -> Worktree {
        Worktree { path: PathBuf::from("/tmp").join(branch), branch: Some(branch.into()), main: false }
    }

    #[test]
    fn query_splits_on_at_and_yields_to_the_branch_field() {
        assert_eq!(split_query(&json!({"query": "data@fix"})), ("data".into(), Some("fix".into())));
        assert_eq!(split_query(&json!({"query": "data"})), ("data".into(), None));
        assert_eq!(
            split_query(&json!({"query": "data@fix", "branch": "other"})),
            ("data@fix".into(), Some("other".into()))
        );
    }

    #[test]
    fn branch_matching_prefers_exact_then_prefix_then_substring() {
        let trees = vec![tree("shared"), tree("shared-extra"), tree("feat/x-shared")];
        assert_eq!(match_branch(&trees, "shared").unwrap().label(), "shared");
        assert_eq!(match_branch(&trees, "shared-e").unwrap().label(), "shared-extra");
        assert_eq!(match_branch(&trees, "x-").unwrap().label(), "feat/x-shared");
        assert!(match_branch(&trees, "share").is_err());
        assert!(match_branch(&trees, "nope").is_err());
    }

    #[test]
    fn worktrees_land_where_zz_puts_them() {
        let repo = PathBuf::from("/Users/e/projects/odei");
        assert_eq!(
            worktree_path(&repo, "feat/zz"),
            PathBuf::from("/Users/e/projects/odei-worktrees/feat-zz")
        );
    }

    #[test]
    fn unknown_actions_and_missing_branches_are_errors() {
        let ctx = ToolContext::new(Path::new("/tmp"));
        assert!(worktree(&ctx, &json!({"action": "sideways"})).is_error);
        assert!(worktree(&ctx, &json!({})).is_error);
        assert!(worktree(&ctx, &json!({"action": "create", "query": "odei"})).is_error);
        assert!(worktree(&ctx, &json!({"action": "run", "query": "odei"})).is_error);
    }

    #[test]
    fn a_repo_reports_its_own_worktrees() {
        let here = std::env::current_dir().unwrap();
        let Ok(repo) = repo_root(&here) else { return };
        let trees = worktrees(&repo).unwrap();
        assert!(trees.iter().any(|w| w.main), "one worktree is always the main checkout");
    }
}

/// Cleanup is the destructive half of the tool, so it is exercised against a
/// real repository rather than a stand-in.
#[cfg(test)]
mod git_tests {
    use super::*;
    use serde_json::json;

    fn scratch_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("odei-test-wt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = dir.join("proj");
        let run = |dir: &Path, args: &[&str]| {
            git(dir, &[&["-c", "user.email=t@t", "-c", "user.name=t"], args].concat()).unwrap()
        };

        git(&dir, &["init", "--bare", "-q", "origin.git"]).unwrap();
        git(&dir, &["clone", "-q", "origin.git", "proj"]).unwrap();
        run(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
        run(&repo, &["branch", "-M", "master"]);
        run(&repo, &["push", "-q", "-u", "origin", "master"]);

        run(&repo, &["checkout", "-q", "-b", "landed"]);
        run(&repo, &["commit", "-q", "--allow-empty", "-m", "work"]);
        run(&repo, &["checkout", "-q", "master"]);
        run(&repo, &["merge", "-q", "--no-ff", "landed", "-m", "merge"]);
        run(&repo, &["push", "-q", "origin", "master"]);

        run(&repo, &["branch", "open", "master"]);
        run(&repo, &["checkout", "-q", "open"]);
        run(&repo, &["commit", "-q", "--allow-empty", "-m", "wip"]);
        run(&repo, &["checkout", "-q", "master"]);
        repo
    }

    fn call(repo: &Path, input: Value) -> String {
        let out = worktree(&ToolContext::new(repo), &input);
        assert!(!out.is_error, "{input}: {}", out.text);
        out.text
    }

    #[test]
    fn only_landed_worktrees_are_retired() {
        let repo = scratch_repo("landed");
        for branch in ["landed", "open", "fresh"] {
            call(&repo, json!({"action": "create", "branch": branch}));
        }
        assert!(call(&repo, json!({"action": "list"})).contains("4 worktree(s)"));

        std::fs::write(worktree_path(&repo, "landed").join("scratch.txt"), "dirty").unwrap();
        let survey = call(&repo, json!({"action": "merged"}));
        assert!(survey.contains("landed\tmerged into origin/master, but 1 uncommitted"), "{survey}");
        assert!(survey.contains("open\tnot merged"), "{survey}");
        assert!(survey.contains("fresh\tno commits of its own"), "{survey}");

        // Nothing is safe to take yet: dirty, unmerged, and never used.
        assert!(call(&repo, json!({"action": "clean"})).contains("nothing to clean up"));

        let cleaned = call(&repo, json!({"action": "clean", "force": true}));
        assert!(cleaned.contains("retired 2 worktree(s)"), "{cleaned}");
        assert!(cleaned.contains("open\tnot merged"), "unmerged work survives force: {cleaned}");
        let left = call(&repo, json!({"action": "list"}));
        assert!(left.contains("2 worktree(s)") && left.contains("open"), "{left}");
        assert!(!worktree_path(&repo, "landed").exists());
    }

    #[test]
    fn a_named_branch_narrows_the_cleanup() {
        let repo = scratch_repo("named");
        call(&repo, json!({"action": "create", "branch": "landed"}));
        call(&repo, json!({"action": "create", "branch": "spare"}));
        let cleaned = call(&repo, json!({"action": "clean", "branch": "landed"}));
        assert!(cleaned.contains("retired 1 worktree(s)"), "{cleaned}");
        assert!(!cleaned.contains("spare"), "{cleaned}");
        assert!(worktree_path(&repo, "spare").is_dir());
    }
}



