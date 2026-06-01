//! Benchmark de motores de git (libgit2 vs gitoxide). Sin UI.
//! Uso:  cargo run -p app --example bench --release -- [ruta-repo] [limit]

use std::time::{Duration, Instant};

fn bench(label: &str, n: usize, dt: Duration) {
    let rate = n as f64 / dt.as_secs_f64();
    println!("  {label:<22} {:>9.2?}   {n} commits  (~{:.0} c/s)", dt, rate);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let limit: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);

    println!("== rebased-rs · benchmark de motor ==");
    println!("repo: {path}   limit: {limit}\n");

    let repo = git_core::Repo::open(&path)?;
    let t = Instant::now();
    let g2 = repo.log(limit)?;
    bench("libgit2 (git2)", g2.len(), t.elapsed());

    for i in 1..=3 {
        let t = Instant::now();
        let gx = git_core::gix_log(&path, limit)?;
        bench(&format!("gitoxide (gix) #{i}"), gx.len(), t.elapsed());
    }
    Ok(())
}
