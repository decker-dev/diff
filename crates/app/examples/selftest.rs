//! Auto-test del MOTOR, headless (sin GUI). Mide cada operación y la compara
//! contra el `git` CLI (la referencia de velocidad). Valida también stage/commit
//! en un repo temporal. Pensado para correr en RELEASE.
//!
//! Uso:  cargo run -p app --example selftest --release -- [repo-grande]

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
