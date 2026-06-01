//! Blame/annotate engine: for each line of a file, which commit introduced it.
//! Uses gix blame (fast on the gitoxide ODB). UI-independent.

use std::collections::HashMap;

/// An annotated line of the file.
#[derive(Debug, Clone)]
pub struct BlameLine {
    pub line_no: u32,
    /// Short hash (8) of the commit that introduced the line.
    pub commit: String,
    pub author: String,
    pub content: String,
}

/// Annotates each line of `file_path` as of `commit_id`.
pub fn blame_file(
    repo_path: &str,
    commit_id: &str,
    file_path: &str,
) -> Result<Vec<BlameLine>, String> {
    use gix::bstr::BStr;

    let mut repo = gix::open(repo_path).map_err(|e| e.to_string())?;
    repo.object_cache_size(32 * 1024 * 1024);

    let suspect = gix::ObjectId::from_hex(commit_id.as_bytes()).map_err(|e| e.to_string())?;
    let outcome = repo
        .blame_file(BStr::new(file_path), suspect, Default::default())
        .map_err(|e| e.to_string())?;

    // File contents, split into lines to get each line's text.
    let lines: Vec<String> = outcome
        .blob
        .split(|&b| b == b'\n')
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect();

    // Per-commit metadata cache (avoids re-looking up the same commit's author).
    let mut meta: HashMap<gix::ObjectId, (String, String)> = HashMap::new();
    let mut out: Vec<BlameLine> = Vec::with_capacity(lines.len());

    for entry in &outcome.entries {
        let cid = entry.commit_id;
        let (short, author) = meta
            .entry(cid)
            .or_insert_with(|| commit_meta(&repo, cid))
            .clone();

        for i in 0..entry.len.get() {
            let blamed_idx = entry.start_in_blamed_file + i;
            let content = lines.get(blamed_idx as usize).cloned().unwrap_or_default();
            out.push(BlameLine {
                line_no: blamed_idx + 1,
                commit: short.clone(),
                author: author.clone(),
                content,
            });
        }
    }

    // Entries don't necessarily come in line order.
    out.sort_by_key(|l| l.line_no);
    Ok(out)
}

/// (short hash, author) of a commit, error-tolerant.
fn commit_meta(repo: &gix::Repository, id: gix::ObjectId) -> (String, String) {
    let full = id.to_string();
    let short = full.get(..8).unwrap_or(&full).to_string();
    let author = repo
        .find_object(id)
        .ok()
        .and_then(|o| o.try_into_commit().ok())
        .and_then(|c| c.author().ok().map(|a| a.name.to_string()))
        .unwrap_or_default();
    (short, author)
}
