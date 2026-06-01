//! Vuelca el diff de un commit (HEAD por defecto) para verificar el motor de diff.
//! Uso:  cargo run -p app --example diffdump -- [ruta-repo] [commit-id]

use git_core::diff::LineOrigin;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let repo = git_core::Repo::open(&path)?;

    let commit = match std::env::args().nth(2) {
        Some(c) => c,
        None => git_core::gix_log(&path, 1)?
            .first()
            .map(|c| c.id.clone())
            .ok_or("repo vacío")?,
    };

    let files = repo.commit_diff(&commit)?;
    println!("diff de {}  →  {} archivos cambiados", &commit[..12.min(commit.len())], files.len());

    for f in &files {
        let (add, del) = f.line_stats();
        let rename = f
            .old_path
            .as_ref()
            .map(|o| format!("  ({o} →)"))
            .unwrap_or_default();
        let bin = if f.binary { "  [binario]" } else { "" };
        println!("\n  {:?}  {}  (+{add} -{del}){rename}{bin}", f.status, f.path);
        if let Some(h) = f.hunks.first() {
            println!("    {}", h.header);
            for l in h.lines.iter().take(6) {
                let sign = match l.origin {
                    LineOrigin::Add => '+',
                    LineOrigin::Del => '-',
                    LineOrigin::Context => ' ',
                };
                println!("    {sign}{}", l.content);
            }
        }
    }
    Ok(())
}
