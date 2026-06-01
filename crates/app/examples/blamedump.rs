//! Checks the blame engine. Usage: cargo run -p app --example blamedump --release -- <repo> <file> [commit]

use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let file = std::env::args().nth(2).unwrap_or_else(|| "README.md".into());
    let commit = match std::env::args().nth(3) {
        Some(c) => c,
        None => git_core::gix_log(&path, 1)?
            .first()
            .map(|c| c.id.clone())
            .ok_or("empty repo")?,
    };

    let t = Instant::now();
    let lines = git_core::blame::blame_file(&path, &commit, &file)?;
    let dt = t.elapsed();

    println!("blame of {file} @ {}  →  {} lines in {:?}\n", &commit[..8], lines.len(), dt);
    for l in lines.iter().take(15) {
        println!("  {:>4} {:8} {:<16} {}", l.line_no, l.commit, trunc(&l.author, 15), trunc(&l.content, 70));
    }
    Ok(())
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}
