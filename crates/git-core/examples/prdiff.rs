//! Headless bench of the PR review load path (what `open_pr` does in the GUI).
//! Usage: cargo run -p git-core --example prdiff --release -- <repo_dir> <pr#>
//! `repo_dir` just needs a git remote pointing at the target GitHub repo; `gh`
//! resolves the PR via the API (no clone of the repo contents required).

use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: prdiff <repo_dir> <pr_number>");
        std::process::exit(1);
    }
    let repo = args[1].clone();
    let num = args[2].clone();

    println!("== PR #{num} ==\n");

    // Each call on its own, serially (the old behavior).
    let t = Instant::now();
    let detail = git_core::gh_pr_detail(&repo, &num);
    let d_detail = t.elapsed();

    let t = Instant::now();
    let diff_text = git_core::gh_pr_diff(&repo, &num).unwrap_or_default();
    let d_diff = t.elapsed();

    let t = Instant::now();
    let comments = git_core::gh_pr_review_comments(&repo, &num).unwrap_or_default();
    let d_com = t.elapsed();

    let t = Instant::now();
    let conversation = git_core::gh_pr_conversation(&repo, &num).unwrap_or_default();
    let d_conv = t.elapsed();

    let t = Instant::now();
    let diff = git_core::diff::parse_unified_diff(&diff_text);
    let d_parse = t.elapsed();

    let files = diff.len();
    let hunks: usize = diff.iter().map(|f| f.hunks.len()).sum();
    let lines: usize = diff.iter().flat_map(|f| &f.hunks).map(|h| h.lines.len()).sum();

    match &detail {
        Ok(d) => println!("detail: \"{}\"  +{}/-{}", d.title, d.additions, d.deletions),
        Err(e) => println!("detail ERROR: {e}"),
    }
    println!("diff loaded:  {} bytes", diff_text.len());
    println!("parsed:       {files} files, {hunks} hunks, {lines} lines");
    println!("comments:     {} inline + {} conversation", comments.len(), conversation.len());

    println!("\n-- SERIAL timings (old) --");
    println!("  detail:           {:>8.2?}", d_detail);
    println!("  diff:             {:>8.2?}", d_diff);
    println!("  review comments:  {:>8.2?}", d_com);
    println!("  conversation:     {:>8.2?}", d_conv);
    println!("  = sum:            {:>8.2?}", d_detail + d_diff + d_com + d_conv);
    println!("  (parse diff:      {:>8.2?})", d_parse);

    // The four loads in PARALLEL (what open_pr does now).
    let t = Instant::now();
    let _ = std::thread::scope(|s| {
        let h1 = s.spawn(|| git_core::gh_pr_detail(&repo, &num).is_ok());
        let h2 = s.spawn(|| git_core::gh_pr_diff(&repo, &num).unwrap_or_default().len());
        let h3 = s.spawn(|| git_core::gh_pr_review_comments(&repo, &num).unwrap_or_default().len());
        let h4 = s.spawn(|| git_core::gh_pr_conversation(&repo, &num).unwrap_or_default().len());
        (h1.join().unwrap(), h2.join().unwrap(), h3.join().unwrap(), h4.join().unwrap())
    });
    let d_par = t.elapsed();

    println!("\n-- the four loads in PARALLEL (new) --");
    println!("  = total:          {:>8.2?}", d_par);
}
