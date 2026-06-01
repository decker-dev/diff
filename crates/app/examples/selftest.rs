//! Auto-test del MOTOR, headless (sin GUI). Mide cada operación y la compara
//! contra el `git` CLI (la referencia de velocidad). Valida también stage/commit
//! en un repo temporal. Pensado para correr en RELEASE.
//!
//! Uso:  cargo run -p app --example selftest --release -- [repo-grande]

use git_core::rebase::{RebaseAction, RebaseResult, RebaseStep};
use std::fs;
use std::process::Command;
use std::time::Instant;

fn ms<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64() * 1000.0)
}

/// Tiempo (ms) de un comando git equivalente, como referencia.
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
        format!("{factor:.1}x más rápido que git")
    } else {
        format!("{:.1}x más lento que git", 1.0 / factor)
    };
    println!("  {op:<22} {ours:>8.1}ms  (git {git:>7.1}ms · {verdict})   {info}");
}

fn main() {
    let repo = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/Users/decker/Documents/projects/diff/rebased".into());

    println!("== rebased-rs · AUTO-TEST DEL MOTOR (release) ==");
    println!("repo: {repo}\n");

    // ---- Historia: log ----
    let (log, t) = ms(|| git_core::gix_log(&repo, 1000).expect("log"));
    row("log(1000)", &format!("{} commits", log.len()), t, git_ms(&repo, &["log", "--oneline", "-n", "1000"]));

    let (log50, t) = ms(|| git_core::gix_log(&repo, 50000).expect("log50k"));
    row("log(50000)", &format!("{} commits", log50.len()), t, git_ms(&repo, &["log", "--oneline", "-n", "50000"]));

    let head = log.first().map(|c| c.id.clone()).expect("HEAD");

    // ---- Historia: diff de un commit (frío y caliente) ----
    let (d, t) = ms(|| git_core::diff::commit_diff(&repo, &head).expect("diff"));
    row("diff(HEAD) frío", &format!("{} archivos", d.len()), t, git_ms(&repo, &["diff-tree", "-p", "-r", &head]));
    let (_d, t) = ms(|| git_core::diff::commit_diff(&repo, &head).expect("diff2"));
    row("diff(HEAD) caliente", "(repo cacheado)", t, 0.0);

    // ---- Historia: blame ----
    for file in ["README.md", "build.txt"] {
        if std::path::Path::new(&repo).join(file).exists() {
            let (b, t) = ms(|| git_core::blame::blame_file(&repo, &head, file));
            let n = b.map(|v| v.len()).unwrap_or(0);
            row(&format!("blame({file})"), &format!("{n} líneas"), t, git_ms(&repo, &["blame", "--", file]));
        }
    }

    // ---- Commits (M4): ciclo stage→commit en repo temporal ----
    println!("\n  -- M4: stage/commit en repo temporal --");
    match test_commit_cycle() {
        Ok(msg) => println!("  ✓ {msg}"),
        Err(e) => println!("  ✗ FALLO: {e}"),
    }

    println!("\n  -- M4: amend / cherry-pick / revert --");
    match test_commit_ops() {
        Ok(msg) => println!("  ✓ {msg}"),
        Err(e) => println!("  ✗ FALLO: {e}"),
    }

    println!("\n  -- M5: ramas (crear/checkout/merge FF/borrar) --");
    match test_branch_ops() {
        Ok(msg) => println!("  ✓ {msg}"),
        Err(e) => println!("  ✗ FALLO: {e}"),
    }

    println!("\n  -- M6: rebase interactivo (squash / drop) --");
    match test_rebase() {
        Ok(msg) => println!("  ✓ {msg}"),
        Err(e) => println!("  ✗ FALLO: {e}"),
    }

    println!("\n  -- M7: remoto (push a bare local + remotes) --");
    match test_remote() {
        Ok(msg) => println!("  ✓ {msg}"),
        Err(e) => println!("  ✗ FALLO: {e}"),
    }

    println!("\n  -- M8: stash (save/list/pop) + ignore --");
    match test_stash_ignore() {
        Ok(msg) => println!("  ✓ {msg}"),
        Err(e) => println!("  ✗ FALLO: {e}"),
    }
}

/// Verifica stash (save→revierte, list, pop→reaplica) e ignore.
fn test_stash_ignore() -> Result<String, String> {
    let dir = std::env::temp_dir().join("rebased-rs-stash");
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

    // cambio no commiteado → stash
    fs::write(dir.join("a.txt"), "uno\ndos\n").map_err(|e| e.to_string())?;
    repo.stash_save("WIP").map_err(es)?;
    let after_stash = fs::read_to_string(dir.join("a.txt")).map_err(|e| e.to_string())?;
    if after_stash != "uno\n" {
        return Err(format!("tras stash, a.txt='{after_stash}' (esperaba revertido)"));
    }
    if repo.stash_list().map_err(es)?.is_empty() {
        return Err("stash_list vacío".into());
    }
    repo.stash_pop().map_err(es)?;
    let after_pop = fs::read_to_string(dir.join("a.txt")).map_err(|e| e.to_string())?;
    if !after_pop.contains("dos") {
        return Err("tras pop, no volvió el cambio".into());
    }

    // ignore
    repo.add_to_gitignore("*.log").map_err(es)?;
    fs::write(dir.join("x.log"), "").map_err(|e| e.to_string())?;
    if !repo.is_ignored("x.log").map_err(es)? {
        return Err("x.log debería estar ignorado".into());
    }

    Ok("stash save/list/pop + ignore (*.log) OK".into())
}

/// Verifica push/remotes contra un remote bare local (sin red ni auth).
fn test_remote() -> Result<String, String> {
    let bare = std::env::temp_dir().join("rebased-rs-remote.git");
    let work = std::env::temp_dir().join("rebased-rs-remote-work");
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
        .ok_or("sin rama HEAD")?;

    repo.push("origin", &default)?;

    let out = Command::new("git")
        .arg("-C")
        .arg(&bare)
        .args(["log", "--oneline"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err("el remote bare no recibió el push".into());
    }

    let remotes = repo.remotes().map_err(es)?;
    if !remotes.iter().any(|r| r == "origin") {
        return Err(format!("remotes={remotes:?}"));
    }

    Ok(format!("push a bare local OK (rama {default}), remotes={remotes:?}"))
}

/// Crea un repo temporal con commits A, B, C (archivos distintos para no chocar).
/// Devuelve (repo, dir, id_A, id_B, id_C).
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

/// Verifica el rebase interactivo: squash (3 commits → 2) y drop (saca C).
fn test_rebase() -> Result<String, String> {
    let step = |id: &str, action: RebaseAction| RebaseStep {
        commit: id.to_string(),
        action,
    };

    // SQUASH: base A, pick B, squash C  →  A + (B+C)
    let (repo, dir, a, b, c) = build_abc("rebased-rs-rb-sq")?;
    let res = repo
        .rebase_interactive(&a, &[step(&b, RebaseAction::Pick), step(&c, RebaseAction::Squash)])
        .map_err(es)?;
    if !matches!(res, RebaseResult::Done(_)) {
        return Err(format!("squash: {res:?}"));
    }
    let log = git_core::gix_log(&dir.to_string_lossy(), 10).map_err(|e| e.to_string())?;
    if log.len() != 2 {
        return Err(format!("squash: esperaba 2 commits, hay {}", log.len()));
    }
    if !dir.join("b.txt").exists() || !dir.join("c.txt").exists() {
        return Err("squash: faltan b.txt/c.txt".into());
    }

    // DROP: base A, pick B, drop C  →  A + B (sin c.txt)
    let (repo2, dir2, a2, b2, c2) = build_abc("rebased-rs-rb-dr")?;
    let res2 = repo2
        .rebase_interactive(&a2, &[step(&b2, RebaseAction::Pick), step(&c2, RebaseAction::Drop)])
        .map_err(es)?;
    if !matches!(res2, RebaseResult::Done(_)) {
        return Err(format!("drop: {res2:?}"));
    }
    let log2 = git_core::gix_log(&dir2.to_string_lossy(), 10).map_err(|e| e.to_string())?;
    if log2.len() != 2 || !dir2.join("b.txt").exists() || dir2.join("c.txt").exists() {
        return Err(format!("drop: len={} (esperaba 2), c.txt no debería existir", log2.len()));
    }

    Ok("squash (3→2, b+c fundidos) + drop (C fuera) OK".into())
}

/// Verifica el ciclo de ramas: crear, checkout, commit, merge fast-forward, borrar.
fn test_branch_ops() -> Result<String, String> {
    let dir = std::env::temp_dir().join("rebased-rs-branch");
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

    // rama por defecto (main/master)
    let default = repo
        .branches()
        .map_err(es)?
        .into_iter()
        .find(|b| b.is_head)
        .map(|b| b.name)
        .ok_or("sin rama HEAD")?;

    repo.create_branch("feat").map_err(es)?;
    let names: Vec<String> = repo.branches().map_err(es)?.into_iter().map(|b| b.name).collect();
    if !names.iter().any(|n| n == "feat") {
        return Err(format!("create_branch: ramas={names:?}"));
    }

    repo.checkout_branch("feat").map_err(es)?;
    fs::write(dir.join("b.txt"), "rama\n").map_err(|e| e.to_string())?;
    repo.stage("b.txt").map_err(es)?;
    repo.commit("c2 en feat").map_err(es)?;

    repo.checkout_branch(&default).map_err(es)?;
    let outcome = repo.merge_branch("feat").map_err(es)?;
    if !matches!(outcome, git_core::MergeOutcome::FastForward(_)) {
        return Err(format!("merge esperaba FastForward, dio {outcome:?}"));
    }
    if !dir.join("b.txt").exists() {
        return Err("tras merge FF, falta b.txt".into());
    }
    repo.delete_branch("feat").map_err(es)?;

    Ok(format!("rama default={default}, feat creada/mergeada/borrada OK"))
}

/// Helper: git_core::Error → String (fn item, reutilizable en map_err).
fn es(e: git_core::Error) -> String {
    e.to_string()
}

/// Verifica amend, cherry-pick y revert en un repo temporal.
fn test_commit_ops() -> Result<String, String> {
    let dir = std::env::temp_dir().join("rebased-rs-ops");
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

    // amend: cambiar el mensaje del HEAD
    e2(repo.amend("c1 enmendado"))?;
    let log = git_core::gix_log(&path, 5).map_err(|x| x.to_string())?;
    if log[0].summary != "c1 enmendado" {
        return Err(format!("amend: summary='{}'", log[0].summary));
    }

    // cherry-pick: commit en rama feat, volver, cherry-pick ese commit
    git(&["checkout", "-q", "-b", "feat"]);
    fs::write(dir.join("b.txt"), "rama\n").map_err(|x| x.to_string())?;
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "en feat"]);
    let feat_head = git_core::gix_log(&path, 1).map_err(|x| x.to_string())?[0].id.clone();
    git(&["checkout", "-q", "-"]); // volver a la rama anterior
    e(repo.cherry_pick(&feat_head))?;
    if !dir.join("b.txt").exists() {
        return Err("cherry-pick no trajo b.txt".into());
    }
    e2(repo.commit("cherry de feat"))?;

    // revert: revertir el último commit → b.txt desaparece
    let last = git_core::gix_log(&path, 1).map_err(|x| x.to_string())?[0].id.clone();
    e(repo.revert_commit(&last))?;
    if dir.join("b.txt").exists() {
        return Err("revert no quitó b.txt".into());
    }

    Ok("amend + cherry-pick + revert OK".into())
}

/// Crea un repo temporal, hace stage+commit con git-core y verifica con el log.
fn test_commit_cycle() -> Result<String, String> {
    let dir = std::env::temp_dir().join("rebased-rs-selftest");
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

    // segundo commit: archivo nuevo
    fs::write(dir.join("b.txt"), "otro\n").map_err(|e| e.to_string())?;
    let st = repo.status().map_err(|e| e.to_string())?;
    if !st.iter().any(|e| e.path == "b.txt") {
        return Err("status no detectó b.txt".into());
    }
    repo.stage("b.txt").map_err(|e| format!("stage b: {e}"))?;
    let c2 = repo.commit("segundo commit").map_err(|e| format!("commit2: {e}"))?;

    let log = git_core::gix_log(&path, 5).map_err(|e| e.to_string())?;
    if log.len() != 2 {
        return Err(format!("esperaba 2 commits, hay {}", log.len()));
    }
    if log[0].summary != "segundo commit" || log[1].summary != "primer commit" {
        return Err(format!("summaries inesperados: {:?}", log.iter().map(|c| &c.summary).collect::<Vec<_>>()));
    }
    Ok(format!("2 commits OK ({}, {})", &c1[..7], &c2[..7]))
}
