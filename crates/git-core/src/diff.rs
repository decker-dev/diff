//! Motor de diff: dado un commit, produce el diff estructurado (por archivo,
//! hunks y líneas con origen +/-/contexto). Independiente de la UI.
//!
//! Se apoya en libgit2 (diferenciar un commit es barato: el coste real está en
//! el log/status, no aquí). La UI consume estos tipos sin saber del motor.

use crate::FileState;

/// Origen de una línea del diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOrigin {
    Context,
    Add,
    Del,
}

/// Una línea dentro de un hunk.
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub origin: LineOrigin,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
    pub content: String,
}

/// Un hunk (`@@ -a,b +c,d @@`) con sus líneas.
#[derive(Debug, Clone)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// El diff de un archivo dentro de un commit.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    /// Ruta anterior si hubo rename.
    pub old_path: Option<String>,
    pub status: FileState,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// (añadidas, borradas) — para el resumen tipo `+12 -3`.
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

/// Diff de un commit contra su primer padre (o el árbol vacío si es root).
pub fn commit_diff(
    repo: &git2::Repository,
    commit_id: &str,
) -> Result<Vec<FileDiff>, git2::Error> {
    let oid = git2::Oid::from_str(commit_id)?;
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;
    let parent_tree = if commit.parent_count() > 0 {
        Some(commit.parent(0)?.tree()?)
    } else {
        None
    };

    let mut opts = git2::DiffOptions::new();
    opts.context_lines(3);
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))?;

    let delta_count = diff.deltas().len();
    let mut files = Vec::with_capacity(delta_count);
    for idx in 0..delta_count {
        let delta = match diff.get_delta(idx) {
            Some(d) => d,
            None => continue,
        };
        let new_path = delta
            .new_file()
            .path()
            .map(|p| p.to_string_lossy().into_owned());
        let old_path = delta
            .old_file()
            .path()
            .map(|p| p.to_string_lossy().into_owned());
        let path = new_path.clone().or_else(|| old_path.clone()).unwrap_or_default();
        // old_path solo es interesante si difiere (rename).
        let old_path = match (&old_path, &new_path) {
            (Some(o), Some(n)) if o != n => Some(o.clone()),
            _ => None,
        };

        let mut hunks = Vec::new();
        let mut binary = false;
        match git2::Patch::from_diff(&diff, idx)? {
            Some(patch) => {
                for h in 0..patch.num_hunks() {
                    let (hunk, line_count) = patch.hunk(h)?;
                    let header = String::from_utf8_lossy(hunk.header())
                        .trim_end()
                        .to_string();
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
            }
            // Sin patch textual: binario.
            None => binary = true,
        }

        files.push(FileDiff {
            path,
            old_path,
            status: map_delta(delta.status()),
            binary,
            hunks,
        });
    }
    Ok(files)
}

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
