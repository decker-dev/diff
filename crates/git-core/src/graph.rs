//! Computes the commit DAG layout ("swimlanes") for the log graph.
//!
//! Given the commit list in topological/date order (newest first), assigns
//! each a *lane* (column) and a stable branch color, and leaves a
//! snapshot of the active lanes per row to draw the vertical lines.
//! Pure, testable logic with no UI dependency.

use crate::CommitInfo;

/// Kind of edge segment within a row's band (top→bottom).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// A lane passing straight through this row (top → bottom, same column).
    Vertical,
    /// A line entering from the top and converging into the commit dot.
    IntoNode,
    /// A line leaving the commit dot toward the bottom (first parent / fork).
    OutOfNode,
}

/// One line segment to draw in a row's graph band.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEdge {
    pub col: usize,
    pub color: u32,
    pub kind: EdgeKind,
}

/// Layout of one graph row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowGraph {
    /// Column (lane) where the commit dot is drawn.
    pub lane: usize,
    /// Dot color (palette index, unbounded; the renderer does `% palette`).
    pub color: u32,
    /// Lanes active when entering this row: `Some(color)` if occupied.
    /// Used to size the gutter.
    pub lanes: Vec<Option<u32>>,
    /// Line segments to draw in this row (vertical lines + merge/fork curves).
    pub edges: Vec<GraphEdge>,
}

impl RowGraph {
    /// Number of occupied lanes in this row (to size the gutter).
    pub fn width(&self) -> usize {
        self.lanes
            .iter()
            .rposition(|l| l.is_some())
            .map_or(0, |i| i + 1)
    }
}

/// Computes the graph layout for `commits` (order: newest first).
pub fn compute_graph(commits: &[CommitInfo]) -> Vec<RowGraph> {
    // Each lane holds the oid of the commit it "expects" to find further down.
    let mut lanes: Vec<Option<&str>> = Vec::new();
    // Color assigned to each lane (parallel to `lanes`).
    let mut colors: Vec<u32> = Vec::new();
    let mut next_color: u32 = 0;
    let mut out = Vec::with_capacity(commits.len());

    for c in commits {
        // 1. Node lane: the one already expecting this commit (a child pointed to it),
        //    or a new one if it's a tip with no children in the loaded window.
        let node_lane = match lanes.iter().position(|l| *l == Some(c.id.as_str())) {
            Some(idx) => idx,
            None => alloc_lane(&mut lanes, &mut colors, &mut next_color),
        };
        let node_color = colors[node_lane];

        // 2. Snapshot of the lanes active in this row (lines crossing it).
        let snapshot: Vec<Option<u32>> = lanes
            .iter()
            .enumerate()
            .map(|(i, l)| l.map(|_| colors[i]))
            .collect();

        let mut edges: Vec<GraphEdge> = Vec::new();

        // Incoming: every lane (at the top) expecting this commit converges into the dot.
        for (i, l) in lanes.iter().enumerate() {
            if *l == Some(c.id.as_str()) {
                edges.push(GraphEdge { col: i, color: colors[i], kind: EdgeKind::IntoNode });
            }
        }

        // 3. Converging merges: other lanes expecting this commit are merged in.
        for i in 0..lanes.len() {
            if i != node_lane && lanes[i] == Some(c.id.as_str()) {
                lanes[i] = None;
            }
        }

        // 4. The first parent continues in the node's lane; with no parents, the lane ends.
        match c.parents.first() {
            Some(p) => lanes[node_lane] = Some(p.as_str()),
            None => lanes[node_lane] = None,
        }
        if lanes[node_lane].is_some() {
            edges.push(GraphEdge { col: node_lane, color: node_color, kind: EdgeKind::OutOfNode });
        }

        // 5. Extra parents (merge): open new lanes if nobody expects them yet.
        for p in c.parents.iter().skip(1) {
            let idx = match lanes.iter().position(|l| *l == Some(p.as_str())) {
                Some(i) => i,
                None => {
                    let i = alloc_lane(&mut lanes, &mut colors, &mut next_color);
                    lanes[i] = Some(p.as_str());
                    i
                }
            };
            edges.push(GraphEdge { col: idx, color: colors[idx], kind: EdgeKind::OutOfNode });
        }

        // Pass-through lanes: occupied both before and after, untouched by this commit.
        for (i, l) in lanes.iter().enumerate() {
            if i != node_lane
                && l.is_some()
                && snapshot.get(i).copied().flatten().is_some()
                && !edges.iter().any(|e| e.col == i)
            {
                edges.push(GraphEdge { col: i, color: colors[i], kind: EdgeKind::Vertical });
            }
        }

        out.push(RowGraph {
            lane: node_lane,
            color: node_color,
            lanes: snapshot,
            edges,
        });
    }
    out
}

/// Reserves the first free lane (or creates one at the end) and gives it a new color.
fn alloc_lane<'a>(
    lanes: &mut Vec<Option<&'a str>>,
    colors: &mut Vec<u32>,
    next_color: &mut u32,
) -> usize {
    let color = *next_color;
    *next_color = next_color.wrapping_add(1);
    match lanes.iter().position(|l| l.is_none()) {
        Some(idx) => {
            colors[idx] = color;
            idx
        }
        None => {
            lanes.push(None);
            colors.push(color);
            lanes.len() - 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(id: &str, parents: &[&str]) -> CommitInfo {
        CommitInfo {
            id: id.to_string(),
            summary: String::new(),
            author: String::new(),
            time: 0,
            parents: parents.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn linear_history_single_lane() {
        let commits = [commit("A", &["B"]), commit("B", &["C"]), commit("C", &[])];
        let g = compute_graph(&commits);
        assert_eq!(g.len(), 3);
        for row in &g {
            assert_eq!(row.lane, 0, "all linear history goes in lane 0");
        }
        // Same branch color along the whole line.
        assert_eq!(g[0].color, g[1].color);
        assert_eq!(g[1].color, g[2].color);
    }

    #[test]
    fn merge_opens_and_closes_second_lane() {
        // M is a merge of A and B; both come from `base`.
        let commits = [
            commit("M", &["A", "B"]),
            commit("A", &["base"]),
            commit("B", &["base"]),
            commit("base", &[]),
        ];
        let g = compute_graph(&commits);
        assert_eq!(g[0].lane, 0, "the merge is in lane 0");
        assert_eq!(g[1].lane, 0, "A continues lane 0 (first parent)");
        assert_eq!(g[2].lane, 1, "B lives in the second lane opened by the merge");
        assert_eq!(g[3].lane, 0, "base reconverges to lane 0");
        // In B's row there are two active lanes (gutter width is 2).
        assert_eq!(g[2].width(), 2);
        // A and B have different branch colors.
        assert_ne!(g[1].color, g[2].color);
    }
}
