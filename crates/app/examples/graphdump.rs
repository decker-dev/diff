//! Dumps the computed graph layout (lane + edges) per row, to debug rendering.
//! Usage: cargo run -p app --example graphdump --release -- <repo>
use git_core::graph::compute_graph;

fn main() {
    let repo = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let commits = git_core::gix_log(&repo, 50).expect("log");
    let g = compute_graph(&commits);
    let width = g.iter().map(|r| r.width()).max().unwrap_or(1);
    println!("width = {width}");
    for (c, r) in commits.iter().zip(g.iter()) {
        let edges: Vec<String> = r
            .edges
            .iter()
            .map(|e| format!("{:?}@{}c{}", e.kind, e.col, e.color))
            .collect();
        println!("lane{} color{}  [{}]  {}", r.lane, r.color, edges.join(", "), c.summary);
    }
}
