//! Mide y vuelca el diff de commits para diagnosticar el rendimiento del clic.
//! Uso:  cargo run -p app --example diffdump --release -- [ruta-repo]

use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let head = git_core::gix_log(&path, 8)?;

    println!("== commit_diff (gix tree-diff + git2 buffers) — lo de cada clic ==\n");
    for (i, c) in head.iter().enumerate() {
        let t = Instant::now();
        let files = git_core::diff::commit_diff(&path, &c.id)?;
        let dt = t.elapsed();

        let lines: usize = files
            .iter()
            .map(|f| f.hunks.iter().map(|h| h.lines.len()).sum::<usize>())
            .sum();
        println!(
            "  #{i} {}  diff={:>8.2?}  ({} archivos, {} líneas)",
            &c.id[..8],
            dt,
            files.len(),
            lines
        );
        if i == 0 {
            for f in files.iter().take(3) {
                let (a, d) = f.line_stats();
                println!("       {:?} {} (+{a} -{d})", f.status, f.path);
            }
        }
    }
    Ok(())
}
