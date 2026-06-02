//! Diff engine: given a commit, produces the structured diff (per file,
//! hunks and lines with origin +/-/context). UI-independent.
//!
//! Performance: the tree-diff (which files changed) is done by **gix**,
//! whose object access is ~20x faster than libgit2 on large repos. The
//! per-blob line diff uses `git2::Patch::from_buffers` over the
//! already-read bytes (in-memory diff, no object store). This takes us from
//! ~460 ms/commit (libgit2 tree-diff) to git-CLI speed.

use crate::FileState;
use std::cell::RefCell;
use std::path::Path;

/// Origin of a diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOrigin {
    Context,
    Add,
    Del,
}

/// A line within a hunk.
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub origin: LineOrigin,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
    pub content: String,
}

/// A hunk (`@@ -a,b +c,d @@`) with its lines.
#[derive(Debug, Clone)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// The diff of one file within a commit.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileState,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// (added, removed) — for the `+12 -3` summary.
    pub fn line_stats(&self) -> (usize, usize) {
        let mut add = 0;
        let mut del = 0;
        for h in &self.hunks {
            for l in &h.lines {
                match l.origin {
                    LineOrigin::Add => add += 1,
                    LineOrigin::Del => del += 1,
                    LineOrigin::Context => {}
                }
            }
        }
        (add, del)
    }
}

thread_local! {
    /// gix repo reused across clicks (same UI thread), with the object cache
    /// warm. Avoids reopening the repo and cooling the cache on each diff.
    static DIFF_REPO: RefCell<Option<(String, gix::Repository)>> = const { RefCell::new(None) };
}

/// Diff of a commit against its first parent (or the empty tree if root).
///
/// Reuses a per-thread cached gix repo: the first diff opens it, the
/// rest reuse the warm object cache. Fast on huge repos.
pub fn commit_diff(path: &str, commit_id: &str) -> Result<Vec<FileDiff>, String> {
    commit_diff_ws(path, commit_id, false)
}

/// Like [`commit_diff`] but optionally ignoring whitespace.
pub fn commit_diff_ws(path: &str, commit_id: &str, ignore_ws: bool) -> Result<Vec<FileDiff>, String> {
    DIFF_REPO.with(|cell| {
        let mut slot = cell.borrow_mut();
        let reopen = !matches!(&*slot, Some((p, _)) if p == path);
        if reopen {
            let mut repo = gix::open(path).map_err(|e| e.to_string())?;
            repo.object_cache_size(64 * 1024 * 1024);
            *slot = Some((path.to_string(), repo));
        }
        commit_diff_in(&slot.as_ref().unwrap().1, commit_id, ignore_ws)
    })
}

fn commit_diff_in(
    repo: &gix::Repository,
    commit_id: &str,
    ignore_ws: bool,
) -> Result<Vec<FileDiff>, String> {
    use gix::object::tree::diff::ChangeDetached as Change;

    let oid = gix::ObjectId::from_hex(commit_id.as_bytes()).map_err(|e| e.to_string())?;
    let commit = repo
        .find_object(oid)
        .map_err(|e| e.to_string())?
        .try_into_commit()
        .map_err(|e| e.to_string())?;
    let tree = commit.tree().map_err(|e| e.to_string())?;

    let parent_tree = match commit.parent_ids().next() {
        Some(pid) => repo
            .find_object(pid.detach())
            .map_err(|e| e.to_string())?
            .try_into_commit()
            .map_err(|e| e.to_string())?
            .tree()
            .map_err(|e| e.to_string())?,
        None => repo.empty_tree(),
    };

    // Tree-diff with gix: the expensive part, now fast.
    let changes = repo
        .diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)
        .map_err(|e| e.to_string())?;

    let mut files = Vec::with_capacity(changes.len());
    let mut opts = git2::DiffOptions::new();
    opts.context_lines(3);
    if ignore_ws {
        opts.ignore_whitespace(true);
    }

    for ch in changes {
        let (path_s, old_path, status, old_id, new_id) = match ch {
            Change::Addition { location, entry_mode, id, .. } => {
                if entry_mode.is_tree() {
                    continue;
                }
                (location.to_string(), None, FileState::New, None, Some(id))
            }
            Change::Deletion { location, entry_mode, id, .. } => {
                if entry_mode.is_tree() {
                    continue;
                }
                (location.to_string(), None, FileState::Deleted, Some(id), None)
            }
            Change::Modification { location, previous_id, id, entry_mode, .. } => {
                if entry_mode.is_tree() {
                    continue;
                }
                (location.to_string(), None, FileState::Modified, Some(previous_id), Some(id))
            }
            Change::Rewrite { location, source_location, source_id, id, .. } => (
                location.to_string(),
                Some(source_location.to_string()),
                FileState::Renamed,
                Some(source_id),
                Some(id),
            ),
        };

        let old_bytes = read_blob(repo, old_id)?;
        let new_bytes = read_blob(repo, new_id)?;

        // In-memory line diff (no object-store access).
        let p = Path::new(&path_s);
        let patch =
            git2::Patch::from_buffers(&old_bytes, Some(p), &new_bytes, Some(p), Some(&mut opts))
                .map_err(|e| e.to_string())?;
        let (hunks, binary) = patch_to_hunks(patch).map_err(|e| e.to_string())?;

        files.push(FileDiff { path: path_s, old_path, status, binary, hunks });
    }
    Ok(files)
}

/// Diff of working-tree changes: unstaged (index vs WT) if `staged=false`,
/// or staged (HEAD vs index) if `staged=true`. For the Local Changes view.
pub fn workdir_diff(path: &str, staged: bool) -> Result<Vec<FileDiff>, String> {
    workdir_diff_ws(path, staged, false)
}

/// Like [`workdir_diff`] but optionally ignoring whitespace.
pub fn workdir_diff_ws(path: &str, staged: bool, ignore_ws: bool) -> Result<Vec<FileDiff>, String> {
    let repo = git2::Repository::open(path).map_err(|e| e.to_string())?;
    let mut opts = git2::DiffOptions::new();
    opts.context_lines(3).include_untracked(true).recurse_untracked_dirs(true);
    if ignore_ws {
        opts.ignore_whitespace(true);
    }

    let diff = if staged {
        let head_tree = repo.head().and_then(|h| h.peel_to_tree()).ok();
        repo.diff_tree_to_index(head_tree.as_ref(), None, Some(&mut opts))
    } else {
        repo.diff_index_to_workdir(None, Some(&mut opts))
    }
    .map_err(|e| e.to_string())?;

    let mut files = Vec::new();
    for idx in 0..diff.deltas().len() {
        let delta = match diff.get_delta(idx) {
            Some(d) => d,
            None => continue,
        };
        let path_s = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (hunks, binary) = match git2::Patch::from_diff(&diff, idx).map_err(|e| e.to_string())? {
            Some(patch) => patch_to_hunks(patch).map_err(|e| e.to_string())?,
            None => (Vec::new(), true),
        };
        files.push(FileDiff {
            path: path_s,
            old_path: None,
            status: map_delta(delta.status()),
            binary,
            hunks,
        });
    }
    Ok(files)
}

/// Reads a blob's bytes by its oid (empty if `None`).
fn read_blob(repo: &gix::Repository, id: Option<gix::ObjectId>) -> Result<Vec<u8>, String> {
    match id {
        Some(id) => Ok(repo.find_object(id).map_err(|e| e.to_string())?.data.clone()),
        None => Ok(Vec::new()),
    }
}

/// Extracts hunks/lines from a `git2::Patch`. No text hunks ⇒ binary.
fn patch_to_hunks(patch: git2::Patch) -> Result<(Vec<Hunk>, bool), git2::Error> {
    let n = patch.num_hunks();
    if n == 0 {
        // May be binary or a mode-only change. We mark it binary if there are deltas.
        return Ok((Vec::new(), true));
    }
    let mut hunks = Vec::with_capacity(n);
    for h in 0..n {
        let (hunk, line_count) = patch.hunk(h)?;
        let header = String::from_utf8_lossy(hunk.header()).trim_end().to_string();
        let mut lines = Vec::with_capacity(line_count);
        for l in 0..line_count {
            let line = patch.line_in_hunk(h, l)?;
            let origin = match line.origin() {
                '+' => LineOrigin::Add,
                '-' => LineOrigin::Del,
                _ => LineOrigin::Context,
            };
            let content = String::from_utf8_lossy(line.content())
                .trim_end_matches('\n')
                .to_string();
            lines.push(DiffLine {
                origin,
                old_lineno: line.old_lineno(),
                new_lineno: line.new_lineno(),
                content,
            });
        }
        hunks.push(Hunk { header, lines });
    }
    Ok((hunks, false))
}

/// Reconstructs a `git apply`-able unified patch for a single hunk of a file.
/// Used to stage/unstage one hunk at a time (partial staging). Best suited to
/// modified files (new/deleted files are staged whole).
pub fn build_hunk_patch(file: &FileDiff, hunk_index: usize) -> Option<String> {
    let hunk = file.hunks.get(hunk_index)?;
    let path = &file.path;
    let mut p = String::new();
    p.push_str(&format!("diff --git a/{path} b/{path}\n"));
    p.push_str(&format!("--- a/{path}\n"));
    p.push_str(&format!("+++ b/{path}\n"));
    // The stored header is the full `@@ -a,b +c,d @@ [section]` line.
    p.push_str(hunk.header.trim_end());
    p.push('\n');
    for l in &hunk.lines {
        let sign = match l.origin {
            LineOrigin::Add => '+',
            LineOrigin::Del => '-',
            LineOrigin::Context => ' ',
        };
        p.push(sign);
        p.push_str(&l.content);
        p.push('\n');
    }
    Some(p)
}

/// Parses a textual unified diff (as produced by `git diff` / `gh pr diff`)
/// into our structured [`FileDiff`] list, computing per-line old/new line
/// numbers. Used to render a PR's diff with comments anchored to lines.
pub fn parse_unified_diff(patch: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut cur: Option<FileDiff> = None;
    let (mut old_no, mut new_no) = (0u32, 0u32);

    for raw in patch.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            if let Some(f) = cur.take() {
                files.push(f);
            }
            cur = Some(FileDiff {
                path: diff_git_new_path(rest),
                old_path: None,
                status: FileState::Modified,
                binary: false,
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(f) = cur.as_mut() else { continue };

        if raw.starts_with("new file mode") {
            f.status = FileState::New;
        } else if raw.starts_with("deleted file mode") {
            f.status = FileState::Deleted;
        } else if let Some(p) = raw.strip_prefix("rename from ") {
            f.old_path = Some(p.to_string());
            f.status = FileState::Renamed;
        } else if raw.starts_with("rename to ") {
            f.status = FileState::Renamed;
        } else if raw.starts_with("Binary files") || raw.starts_with("GIT binary patch") {
            f.binary = true;
        } else if let Some(p) = raw.strip_prefix("+++ ") {
            let pp = p.trim();
            if pp != "/dev/null" {
                f.path = pp.strip_prefix("b/").unwrap_or(pp).to_string();
            }
        } else if raw.starts_with("--- ") {
            // old-path marker; the path comes from `diff --git`/`+++`.
        } else if raw.starts_with("@@") {
            if let Some((o, n)) = parse_hunk_header(raw) {
                old_no = o;
                new_no = n;
            }
            f.hunks.push(Hunk { header: raw.to_string(), lines: Vec::new() });
        } else if let Some(hunk) = f.hunks.last_mut() {
            let (origin, content) = match raw.as_bytes().first() {
                Some(b'+') => (LineOrigin::Add, &raw[1..]),
                Some(b'-') => (LineOrigin::Del, &raw[1..]),
                Some(b' ') => (LineOrigin::Context, &raw[1..]),
                // "\ No newline at end of file" and any stray lines: skip.
                _ => continue,
            };
            let (old_lineno, new_lineno) = match origin {
                LineOrigin::Add => {
                    let n = new_no;
                    new_no += 1;
                    (None, Some(n))
                }
                LineOrigin::Del => {
                    let o = old_no;
                    old_no += 1;
                    (Some(o), None)
                }
                LineOrigin::Context => {
                    let (o, n) = (old_no, new_no);
                    old_no += 1;
                    new_no += 1;
                    (Some(o), Some(n))
                }
            };
            hunk.lines.push(DiffLine { origin, old_lineno, new_lineno, content: content.to_string() });
        }
    }
    if let Some(f) = cur.take() {
        files.push(f);
    }
    files
}

/// From a `diff --git a/<old> b/<new>` tail, returns `<new>`.
fn diff_git_new_path(rest: &str) -> String {
    if let Some(idx) = rest.find(" b/") {
        return rest[idx + 3..].trim().to_string();
    }
    rest.trim().trim_start_matches("a/").to_string()
}

/// Parses the start line numbers from a `@@ -a,b +c,d @@` header → (a, c).
fn parse_hunk_header(h: &str) -> Option<(u32, u32)> {
    let minus = h.find('-')?;
    let plus = h.find('+')?;
    let old = h[minus + 1..].split([',', ' ']).next()?.parse().ok()?;
    let new = h[plus + 1..].split([',', ' ']).next()?.parse().ok()?;
    Some((old, new))
}

/// Translates libgit2's `Delta` to our `FileState`.
fn map_delta(d: git2::Delta) -> FileState {
    use git2::Delta;
    match d {
        Delta::Added => FileState::New,
        Delta::Deleted => FileState::Deleted,
        Delta::Renamed => FileState::Renamed,
        Delta::Typechange => FileState::TypeChange,
        Delta::Conflicted => FileState::Conflicted,
        _ => FileState::Modified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unified_diff_with_line_numbers() {
        let patch = "\
diff --git a/src/x.rs b/src/x.rs
index 111..222 100644
--- a/src/x.rs
+++ b/src/x.rs
@@ -1,4 +1,5 @@
 ctx1
-old line
+new line
+added line
 ctx2
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
";
        let files = parse_unified_diff(patch);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/x.rs");
        assert_eq!(files[0].status, FileState::Modified);
        let h = &files[0].hunks[0];
        // The deletion is old line 2; the first addition is new line 2.
        let del = h.lines.iter().find(|l| l.origin == LineOrigin::Del).unwrap();
        assert_eq!(del.old_lineno, Some(2));
        let add = h.lines.iter().find(|l| l.origin == LineOrigin::Add).unwrap();
        assert_eq!(add.new_lineno, Some(2));
        // Context "ctx2" is old line 3 / new line 4.
        let ctx2 = h.lines.iter().find(|l| l.content == "ctx2").unwrap();
        assert_eq!((ctx2.old_lineno, ctx2.new_lineno), (Some(3), Some(4)));

        assert_eq!(files[1].path, "new.txt");
        assert_eq!(files[1].status, FileState::New);
        assert_eq!(files[1].hunks[0].lines[0].new_lineno, Some(1));
    }
}
