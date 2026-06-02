//! ENGINE self-test, headless (no GUI). Measures each operation and compares
//! against the `git` CLI (the speed reference). Also validates stage/commit
//! in a temp repo. Meant to run in RELEASE.
//!
//! Usage:  cargo run -p app --example selftest --release -- [large-repo]

use git_core::rebase::{RebaseAction, RebaseResult, RebaseStep};
use std::fs;
use std::process::Command;
use std::time::Instant;

fn ms<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64() * 1000.0)
}

/// Time (ms) of an equivalent git command, as a reference.
fn git_ms(repo: &str, args: &[&str]) -> f64 {
    let t = Instant::now();
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output();
    t.elapsed().as_secs_f64() * 1000.0
}

fn row(op: &str, info: &str, ours: f64, git: f64) {
    let factor = if ours > 0.0 { git / ours } else { 0.0 };
    let verdict = if factor >= 1.0 {
        format!("{factor:.1}x faster than git")
    } else {
        format!("{:.1}x slower than git", 1.0 / factor)
    };
    println!("  {op:<22} {ours:>8.1}ms  (git {git:>7.1}ms · {verdict})   {info}");
}

fn main() {
    let repo = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/Users/decker/Documents/projects/diff/rebased".into());

    println!("== diff · ENGINE SELF-TEST (release) ==");
    println!("repo: {repo}\n");

    // ---- History: log ----
    let (log, t) = ms(|| git_core::gix_log(&repo, 1000).expect("log"));
    row("log(1000)", &format!("{} commits", log.len()), t, git_ms(&repo, &["log", "--oneline", "-n", "1000"]));

    let (log50, t) = ms(|| git_core::gix_log(&repo, 50000).expect("log50k"));
    row("log(50000)", &format!("{} commits", log50.len()), t, git_ms(&repo, &["log", "--oneline", "-n", "50000"]));

    let head = log.first().map(|c| c.id.clone()).expect("HEAD");

    // ---- History: commit diff (cold and warm) ----
    let (d, t) = ms(|| git_core::diff::commit_diff(&repo, &head).expect("diff"));
    row("diff(HEAD) cold", &format!("{} files", d.len()), t, git_ms(&repo, &["diff-tree", "-p", "-r", &head]));
    let (_d, t) = ms(|| git_core::diff::commit_diff(&repo, &head).expect("diff2"));
    row("diff(HEAD) warm", "(cached repo)", t, 0.0);

    // ---- History: blame ----
    for file in ["README.md", "build.txt"] {
        if std::path::Path::new(&repo).join(file).exists() {
            let (b, t) = ms(|| git_core::blame::blame_file(&repo, &head, file));
            let n = b.map(|v| v.len()).unwrap_or(0);
            row(&format!("blame({file})"), &format!("{n} lines"), t, git_ms(&repo, &["blame", "--", file]));
        }
    }

    // ---- Commits (M4): stage→commit cycle in a temp repo ----
    println!("\n  -- M4: stage/commit in a temp repo --");
    match test_commit_cycle() {
        Ok(msg) => println!("  ✓ {msg}"),
        Err(e) => println!("  ✗ FAILED: {e}"),
    }

    println!("\n  -- M4: amend / cherry-pick / revert --");
    match test_commit_ops() {
        Ok(msg) => println!("  ✓ {msg}"),
        Err(e) => println!("  ✗ FAILED: {e}"),
    }

    println!("\n  -- M5: branches (create/checkout/merge FF/delete) --");
    match test_branch_ops() {
        Ok(msg) => println!("  ✓ {msg}"),
        Err(e) => println!("  ✗ FAILED: {e}"),
    }

    println!("\n  -- M6: interactive rebase (squash / drop) --");
    match test_rebase() {
        Ok(msg) => println!("  ✓ {msg}"),
        Err(e) => println!("  ✗ FAILED: {e}"),
    }

    println!("\n  -- M7: remote (push to local bare + remotes) --");
    match test_remote() {
        Ok(msg) => println!("  ✓ {msg}"),
        Err(e) => println!("  ✗ FAILED: {e}"),
    }

    println!("\n  -- M8: stash (save/list/pop) + ignore --");
    match test_stash_ignore() {
        Ok(msg) => println!("  ✓ {msg}"),
        Err(e) => println!("  ✗ FAILED: {e}"),
    }

    println!("\n  -- PARITY: hunk staging / reset / tags / reflog / conflicts / history+pickaxe --");
    let parity: [(&str, Result<String, String>); 6] = [
        ("hunk staging", test_hunk_staging()),
        ("reset soft/hard", test_reset()),
        ("tags", test_tags()),
        ("reflog", test_reflog()),
        ("conflicts 3-way", test_conflicts()),
        ("history + pickaxe", test_history_pickaxe()),
    ];
    for (name, r) in parity {
        match r {
            Ok(msg) => println!("  ✓ {name}: {msg}"),
            Err(e) => println!("  ✗ {name} FAILED: {e}"),
        }
    }
}

/// Fresh temp repo with git identity configured.
fn fresh_repo(name: &str) -> Result<(git_core::Repo, std::path::PathBuf, String), String> {
    let dir = std::env::temp_dir().join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.to_string_lossy().to_string();
    let git = |args: &[&str]| {
        Command::new("git").arg("-C").arg(&path).args(args).output().ok();
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@rebased.rs"]);
    git(&["config", "user.name", "t"]);
    let repo = git_core::Repo::open(&path).map_err(es)?;
    Ok((repo, dir, path))
}

/// Partial staging: stage exactly one of two hunks via build_hunk_patch + apply.
fn test_hunk_staging() -> Result<String, String> {
    let (repo, dir, path) = fresh_repo("diff-hunk")?;
    let base: String = (1..=20).map(|i| format!("line{i}\n")).collect();
    fs::write(dir.join("f.txt"), &base).map_err(|e| e.to_string())?;
    repo.stage("f.txt").map_err(es)?;
    repo.commit("base").map_err(es)?;

    // Two separated edits → two distinct hunks.
    let edited = base
        .replace("line2\n", "line2-edit\n")
        .replace("line18\n", "line18-edit\n");
    fs::write(dir.join("f.txt"), &edited).map_err(|e| e.to_string())?;

    let before = git_core::diff::workdir_diff(&path, false)?;
    let fd = before.iter().find(|f| f.path == "f.txt").ok_or("f.txt not in diff")?;
    if fd.hunks.len() < 2 {
        return Err(format!("expected 2 hunks, got {}", fd.hunks.len()));
    }
    let patch = git_core::diff::build_hunk_patch(fd, 0).ok_or("could not build hunk patch")?;
    repo.apply_hunk_to_index(&patch, false)?;

    let staged = git_core::diff::workdir_diff(&path, true)?;
    let unstaged = git_core::diff::workdir_diff(&path, false)?;
    let sh = staged.iter().find(|f| f.path == "f.txt").map_or(0, |f| f.hunks.len());
    let uh = unstaged.iter().find(|f| f.path == "f.txt").map_or(0, |f| f.hunks.len());
    if sh < 1 || uh < 1 {
        return Err(format!("partial stage failed (staged hunks={sh}, unstaged hunks={uh})"));
    }
    Ok(format!("staged 1 of 2 hunks (staged={sh}, remaining unstaged={uh})"))
}

/// File history (`--follow`) + pickaxe (`-S`): the file's commits are isolated
/// from unrelated ones, and pickaxe pinpoints the commit that introduced a token.
fn test_history_pickaxe() -> Result<String, String> {
    let (repo, dir, path) = fresh_repo("diff-hist")?;
    fs::write(dir.join("a.txt"), "alpha\n").map_err(|e| e.to_string())?;
    repo.stage("a.txt").map_err(es)?;
    repo.commit("add a").map_err(es)?;

    fs::write(dir.join("a.txt"), "alpha\nUNIQUE_TOKEN\n").map_err(|e| e.to_string())?;
    repo.stage("a.txt").map_err(es)?;
    repo.commit("add token to a").map_err(es)?;

    // Unrelated commit — must NOT show up in a.txt's history.
    fs::write(dir.join("b.txt"), "beta\n").map_err(|e| e.to_string())?;
    repo.stage("b.txt").map_err(es)?;
    repo.commit("add b").map_err(es)?;

    let hist = git_core::file_history(&path, "a.txt", 100)?;
    if hist.len() != 2 {
        return Err(format!("expected 2 commits in a.txt history, got {}", hist.len()));
    }
    if hist.iter().any(|c| c.summary == "add b") {
        return Err("unrelated commit leaked into file history".into());
    }

    let hits = git_core::pickaxe(&path, "UNIQUE_TOKEN", false, 100)?;
    if hits.len() != 1 || hits[0].summary != "add token to a" {
        return Err(format!(
            "pickaxe -S expected 1 hit, got {:?}",
            hits.iter().map(|c| c.summary.clone()).collect::<Vec<_>>()
        ));
    }
    if !git_core::pickaxe(&path, "NOT_PRESENT_ANYWHERE", false, 100)?.is_empty() {
        return Err("pickaxe found a phantom match".into());
    }
    Ok(format!("history={} commits (isolated), pickaxe -S=1 hit", hist.len()))
}

/// reset --soft keeps changes staged; reset --hard discards them.
fn test_reset() -> Result<String, String> {
    let (repo, dir, path) = fresh_repo("diff-reset")?;
    fs::write(dir.join("a.txt"), "a\n").map_err(|e| e.to_string())?;
    repo.stage("a.txt").map_err(es)?;
    repo.commit("A").map_err(es)?;
    let a = git_core::gix_log(&path, 1).map_err(|e| e.to_string())?[0].id.clone();
    fs::write(dir.join("b.txt"), "b\n").map_err(|e| e.to_string())?;
    repo.stage("b.txt").map_err(es)?;
    repo.commit("B").map_err(es)?;

    repo.reset(&a, git_core::ResetMode::Soft).map_err(es)?;
    let log = git_core::gix_log(&path, 10).map_err(|e| e.to_string())?;
    if log.len() != 1 {
        return Err(format!("soft: expected 1 commit reachable, got {}", log.len()));
    }
    let st = repo.status().map_err(es)?;
    if !st.iter().any(|e| e.path == "b.txt" && e.staged) {
        return Err("soft: b.txt should remain staged".into());
    }
    repo.reset(&a, git_core::ResetMode::Hard).map_err(es)?;
    if dir.join("b.txt").exists() {
        return Err("hard: b.txt should be gone".into());
    }
    Ok("soft keeps staged, hard discards".into())
}

/// Tags: lightweight + annotated, chips via refs_by_commit, delete.
fn test_tags() -> Result<String, String> {
    let (repo, dir, path) = fresh_repo("diff-tags")?;
    fs::write(dir.join("a.txt"), "a\n").map_err(|e| e.to_string())?;
    repo.stage("a.txt").map_err(es)?;
    repo.commit("A").map_err(es)?;
    let a = git_core::gix_log(&path, 1).map_err(|e| e.to_string())?[0].id.clone();
    repo.create_tag("v1", &a, None).map_err(es)?;
    repo.create_tag("v2", &a, Some("annotated")).map_err(es)?;
    let tags = repo.tags().map_err(es)?;
    if !tags.iter().any(|t| t.name == "v1") || !tags.iter().any(|t| t.name == "v2" && t.message == "annotated") {
        return Err(format!("tags missing/wrong: {:?}", tags.iter().map(|t| t.name.clone()).collect::<Vec<_>>()));
    }
    let refs = repo.refs_by_commit().map_err(es)?;
    let has_tag = refs.get(&a).is_some_and(|v| v.iter().any(|r| matches!(r.kind, git_core::RefKind::Tag)));
    if !has_tag {
        return Err("refs_by_commit missing tag chip".into());
    }
    repo.delete_tag("v1").map_err(es)?;
    if repo.tags().map_err(es)?.iter().any(|t| t.name == "v1") {
        return Err("v1 not deleted".into());
    }
    Ok("lightweight + annotated + chip + delete OK".into())
}

/// Reflog: non-empty, newest entry is HEAD.
fn test_reflog() -> Result<String, String> {
    let (repo, dir, path) = fresh_repo("diff-reflog")?;
    for f in ["a", "b", "c"] {
        fs::write(dir.join(format!("{f}.txt")), format!("{f}\n")).map_err(|e| e.to_string())?;
        repo.stage(&format!("{f}.txt")).map_err(es)?;
        repo.commit(&format!("commit {f}")).map_err(es)?;
    }
    let rl = repo.reflog(10).map_err(es)?;
    if rl.is_empty() {
        return Err("reflog empty".into());
    }
    let head = git_core::gix_log(&path, 1).map_err(|e| e.to_string())?[0].id.clone();
    if rl[0].id != head {
        return Err(format!("reflog[0]={} but HEAD={}", &rl[0].id[..7], &head[..7]));
    }
    Ok(format!("{} entries, newest == HEAD", rl.len()))
}

/// 3-way conflict: produce one, read base/ours/theirs, resolve with ours.
fn test_conflicts() -> Result<String, String> {
    let (repo, dir, path) = fresh_repo("diff-confl")?;
    fs::write(dir.join("f.txt"), "base\n").map_err(|e| e.to_string())?;
    repo.stage("f.txt").map_err(es)?;
    repo.commit("base").map_err(es)?;
    let default = repo
        .branches()
        .map_err(es)?
        .into_iter()
        .find(|b| b.is_head)
        .map(|b| b.name)
        .ok_or("no HEAD branch")?;

    repo.create_branch("feat").map_err(es)?;
    repo.checkout_branch("feat").map_err(es)?;
    fs::write(dir.join("f.txt"), "theirs\n").map_err(|e| e.to_string())?;
    repo.stage("f.txt").map_err(es)?;
    repo.commit("feat change").map_err(es)?;

    repo.checkout_branch(&default).map_err(es)?;
    fs::write(dir.join("f.txt"), "ours\n").map_err(|e| e.to_string())?;
    repo.stage("f.txt").map_err(es)?;
    repo.commit("main change").map_err(es)?;

    let outcome = repo.merge_branch("feat").map_err(es)?;
    if outcome != git_core::MergeOutcome::Conflicts {
        return Err(format!("expected Conflicts, got {outcome:?}"));
    }
    let confl = repo.conflicts().map_err(es)?;
    if !confl.iter().any(|f| f == "f.txt") {
        return Err(format!("conflicts={confl:?}"));
    }
    let sides = repo.conflict_sides("f.txt").map_err(es)?;
    if sides.ours.as_deref() != Some("ours\n") || sides.theirs.as_deref() != Some("theirs\n") {
        return Err(format!("sides ours={:?} theirs={:?}", sides.ours, sides.theirs));
    }
    repo.resolve_conflict("f.txt", true)?;
    // The app re-opens the repo on every refresh; do the same so the git2
    // in-memory index isn't stale after the CLI add.
    let repo2 = git_core::Repo::open(&path).map_err(es)?;
    let after = repo2.conflicts().map_err(es)?;
    if !after.is_empty() {
        return Err(format!("after resolve, still conflicted: {after:?}"));
    }
    Ok("conflict → 3 sides (ours/theirs read) → resolve ours OK".into())
}

/// Checks stash (save→reverts, list, pop→reapplies) and ignore.
fn test_stash_ignore() -> Result<String, String> {
    let dir = std::env::temp_dir().join("diff-stash");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.to_string_lossy().to_string();
    let git = |args: &[&str]| {
        Command::new("git").arg("-C").arg(&path).args(args).output().ok();
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@rebased.rs"]);
    git(&["config", "user.name", "t"]);

    fs::write(dir.join("a.txt"), "uno\n").map_err(|e| e.to_string())?;
    let mut repo = git_core::Repo::open(&path).map_err(es)?;
    repo.stage("a.txt").map_err(es)?;
    repo.commit("c1").map_err(es)?;

    // uncommitted change → stash
    fs::write(dir.join("a.txt"), "uno\ndos\n").map_err(|e| e.to_string())?;
    repo.stash_save("WIP").map_err(es)?;
    let after_stash = fs::read_to_string(dir.join("a.txt")).map_err(|e| e.to_string())?;
    if after_stash != "uno\n" {
        return Err(format!("after stash, a.txt='{after_stash}' (expected reverted)"));
    }
    if repo.stash_list().map_err(es)?.is_empty() {
        return Err("stash_list empty".into());
    }
    repo.stash_pop().map_err(es)?;
    let after_pop = fs::read_to_string(dir.join("a.txt")).map_err(|e| e.to_string())?;
    if !after_pop.contains("dos") {
        return Err("after pop, the change did not return".into());
    }

    // ignore
    repo.add_to_gitignore("*.log").map_err(es)?;
    fs::write(dir.join("x.log"), "").map_err(|e| e.to_string())?;
    if !repo.is_ignored("x.log").map_err(es)? {
        return Err("x.log should be ignored".into());
    }

    Ok("stash save/list/pop + ignore (*.log) OK".into())
}

/// Checks push/remotes against a local bare remote (no network or auth).
fn test_remote() -> Result<String, String> {
    let bare = std::env::temp_dir().join("diff-remote.git");
    let work = std::env::temp_dir().join("diff-remote-work");
    let _ = fs::remove_dir_all(&bare);
    let _ = fs::remove_dir_all(&work);
    let bare_path = bare.to_string_lossy().to_string();

    Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(&bare)
        .output()
        .map_err(|e| e.to_string())?;

    fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let work_path = work.to_string_lossy().to_string();
    let git = |args: &[&str]| {
        Command::new("git").arg("-C").arg(&work_path).args(args).output().ok();
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@rebased.rs"]);
    git(&["config", "user.name", "t"]);
    git(&["remote", "add", "origin", &bare_path]);

    fs::write(work.join("a.txt"), "uno\n").map_err(|e| e.to_string())?;
    let repo = git_core::Repo::open(&work_path).map_err(es)?;
    repo.stage("a.txt").map_err(es)?;
    repo.commit("c1").map_err(es)?;

    let default = repo
        .branches()
        .map_err(es)?
        .into_iter()
        .find(|b| b.is_head)
        .map(|b| b.name)
        .ok_or("no HEAD branch")?;

    repo.push("origin", &default)?;

    let out = Command::new("git")
        .arg("-C")
        .arg(&bare)
        .args(["log", "--oneline"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err("the bare remote did not receive the push".into());
    }

    let remotes = repo.remotes().map_err(es)?;
    if !remotes.iter().any(|r| r == "origin") {
        return Err(format!("remotes={remotes:?}"));
    }

    Ok(format!("push to local bare OK (branch {default}), remotes={remotes:?}"))
}

/// Creates a temp repo with commits A, B, C (distinct files to avoid conflicts).
/// Returns (repo, dir, id_A, id_B, id_C).
fn build_abc(name: &str) -> Result<(git_core::Repo, std::path::PathBuf, String, String, String), String> {
    let dir = std::env::temp_dir().join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.to_string_lossy().to_string();
    let git = |args: &[&str]| {
        Command::new("git").arg("-C").arg(&path).args(args).output().ok();
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@rebased.rs"]);
    git(&["config", "user.name", "t"]);
    let repo = git_core::Repo::open(&path).map_err(es)?;

    let commit_file = |f: &str, msg: &str| -> Result<String, String> {
        fs::write(dir.join(f), format!("{f}\n")).map_err(|e| e.to_string())?;
        repo.stage(f).map_err(es)?;
        repo.commit(msg).map_err(es)?;
        Ok(git_core::gix_log(&path, 1).map_err(|e| e.to_string())?[0].id.clone())
    };
    let a = commit_file("a.txt", "A")?;
    let b = commit_file("b.txt", "B")?;
    let c = commit_file("c.txt", "C")?;
    drop(commit_file);
    Ok((repo, dir, a, b, c))
}

/// Checks interactive rebase: squash (3 commits → 2) and drop (removes C).
fn test_rebase() -> Result<String, String> {
    let step = |id: &str, action: RebaseAction| RebaseStep {
        commit: id.to_string(),
        action,
    };

    // SQUASH: base A, pick B, squash C  →  A + (B+C)
    let (repo, dir, a, b, c) = build_abc("diff-rb-sq")?;
    let res = repo
        .rebase_interactive(&a, &[step(&b, RebaseAction::Pick), step(&c, RebaseAction::Squash)])
        .map_err(es)?;
    if !matches!(res, RebaseResult::Done(_)) {
        return Err(format!("squash: {res:?}"));
    }
    let log = git_core::gix_log(&dir.to_string_lossy(), 10).map_err(|e| e.to_string())?;
    if log.len() != 2 {
        return Err(format!("squash: expected 2 commits, got {}", log.len()));
    }
    if !dir.join("b.txt").exists() || !dir.join("c.txt").exists() {
        return Err("squash: b.txt/c.txt missing".into());
    }

    // DROP: base A, pick B, drop C  →  A + B (no c.txt)
    let (repo2, dir2, a2, b2, c2) = build_abc("diff-rb-dr")?;
    let res2 = repo2
        .rebase_interactive(&a2, &[step(&b2, RebaseAction::Pick), step(&c2, RebaseAction::Drop)])
        .map_err(es)?;
    if !matches!(res2, RebaseResult::Done(_)) {
        return Err(format!("drop: {res2:?}"));
    }
    let log2 = git_core::gix_log(&dir2.to_string_lossy(), 10).map_err(|e| e.to_string())?;
    if log2.len() != 2 || !dir2.join("b.txt").exists() || dir2.join("c.txt").exists() {
        return Err(format!("drop: len={} (expected 2), c.txt should not exist", log2.len()));
    }

    Ok("squash (3→2, b+c melded) + drop (C removed) OK".into())
}

/// Checks the branch cycle: create, checkout, commit, fast-forward merge, delete.
fn test_branch_ops() -> Result<String, String> {
    let dir = std::env::temp_dir().join("diff-branch");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.to_string_lossy().to_string();
    let git = |args: &[&str]| {
        Command::new("git").arg("-C").arg(&path).args(args).output().ok();
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@rebased.rs"]);
    git(&["config", "user.name", "t"]);

    fs::write(dir.join("a.txt"), "uno\n").map_err(|e| e.to_string())?;
    let repo = git_core::Repo::open(&path).map_err(es)?;
    repo.stage("a.txt").map_err(es)?;
    repo.commit("c1").map_err(es)?;

    // default branch (main/master)
    let default = repo
        .branches()
        .map_err(es)?
        .into_iter()
        .find(|b| b.is_head)
        .map(|b| b.name)
        .ok_or("no HEAD branch")?;

    repo.create_branch("feat").map_err(es)?;
    let names: Vec<String> = repo.branches().map_err(es)?.into_iter().map(|b| b.name).collect();
    if !names.iter().any(|n| n == "feat") {
        return Err(format!("create_branch: branches={names:?}"));
    }

    repo.checkout_branch("feat").map_err(es)?;
    fs::write(dir.join("b.txt"), "branch\n").map_err(|e| e.to_string())?;
    repo.stage("b.txt").map_err(es)?;
    repo.commit("c2 en feat").map_err(es)?;

    repo.checkout_branch(&default).map_err(es)?;
    let outcome = repo.merge_branch("feat").map_err(es)?;
    if !matches!(outcome, git_core::MergeOutcome::FastForward(_)) {
        return Err(format!("merge expected FastForward, got {outcome:?}"));
    }
    if !dir.join("b.txt").exists() {
        return Err("after FF merge, b.txt missing".into());
    }
    repo.delete_branch("feat").map_err(es)?;

    Ok(format!("default branch={default}, feat created/merged/deleted OK"))
}

/// Helper: git_core::Error → String (fn item, reusable in map_err).
fn es(e: git_core::Error) -> String {
    e.to_string()
}

/// Checks amend, cherry-pick and revert in a temp repo.
fn test_commit_ops() -> Result<String, String> {
    let dir = std::env::temp_dir().join("diff-ops");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.to_string_lossy().to_string();
    let git = |args: &[&str]| {
        Command::new("git").arg("-C").arg(&path).args(args).output().ok();
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@rebased.rs"]);
    git(&["config", "user.name", "t"]);

    let e = |r: Result<(), git_core::Error>| r.map_err(|e| e.to_string());
    let e2 = |r: Result<String, git_core::Error>| r.map_err(|e| e.to_string());

    fs::write(dir.join("a.txt"), "uno\n").map_err(|x| x.to_string())?;
    let repo = git_core::Repo::open(&path).map_err(|x| x.to_string())?;
    e(repo.stage("a.txt"))?;
    e2(repo.commit("c1"))?;

    // amend: change HEAD's message
    e2(repo.amend("c1 enmendado"))?;
    let log = git_core::gix_log(&path, 5).map_err(|x| x.to_string())?;
    if log[0].summary != "c1 enmendado" {
        return Err(format!("amend: summary='{}'", log[0].summary));
    }

    // cherry-pick: commit on feat branch, go back, cherry-pick it
    git(&["checkout", "-q", "-b", "feat"]);
    fs::write(dir.join("b.txt"), "branch\n").map_err(|x| x.to_string())?;
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "en feat"]);
    let feat_head = git_core::gix_log(&path, 1).map_err(|x| x.to_string())?[0].id.clone();
    git(&["checkout", "-q", "-"]); // back to the previous branch
    e(repo.cherry_pick(&feat_head))?;
    if !dir.join("b.txt").exists() {
        return Err("cherry-pick did not bring b.txt".into());
    }
    e2(repo.commit("cherry de feat"))?;

    // revert: revert the last commit → b.txt disappears
    let last = git_core::gix_log(&path, 1).map_err(|x| x.to_string())?[0].id.clone();
    e(repo.revert_commit(&last))?;
    if dir.join("b.txt").exists() {
        return Err("revert did not remove b.txt".into());
    }

    Ok("amend + cherry-pick + revert OK".into())
}

/// Creates a temp repo, stages+commits with git-core and verifies via the log.
fn test_commit_cycle() -> Result<String, String> {
    let dir = std::env::temp_dir().join("diff-selftest");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.to_string_lossy().to_string();

    let run = |args: &[&str]| {
        Command::new("git").arg("-C").arg(&path).args(args).output().ok();
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@rebased.rs"]);
    run(&["config", "user.name", "selftest"]);

    fs::write(dir.join("a.txt"), "linea 1\n").map_err(|e| e.to_string())?;
    let repo = git_core::Repo::open(&path).map_err(|e| e.to_string())?;

    repo.stage("a.txt").map_err(|e| format!("stage: {e}"))?;
    let c1 = repo.commit("primer commit").map_err(|e| format!("commit: {e}"))?;

    // second commit: new file
    fs::write(dir.join("b.txt"), "otro\n").map_err(|e| e.to_string())?;
    let st = repo.status().map_err(|e| e.to_string())?;
    if !st.iter().any(|e| e.path == "b.txt") {
        return Err("status did not detect b.txt".into());
    }
    repo.stage("b.txt").map_err(|e| format!("stage b: {e}"))?;
    let c2 = repo.commit("segundo commit").map_err(|e| format!("commit2: {e}"))?;

    let log = git_core::gix_log(&path, 5).map_err(|e| e.to_string())?;
    if log.len() != 2 {
        return Err(format!("expected 2 commits, got {}", log.len()));
    }
    if log[0].summary != "segundo commit" || log[1].summary != "primer commit" {
        return Err(format!("unexpected summaries: {:?}", log.iter().map(|c| &c.summary).collect::<Vec<_>>()));
    }
    Ok(format!("2 commits OK ({}, {})", &c1[..7], &c2[..7]))
}
