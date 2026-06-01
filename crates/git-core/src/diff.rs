//! Motor de diff: dado un commit, produce el diff estructurado (por archivo,
//! hunks y líneas con origen +/-/contexto). Independiente de la UI.
//!
//! Rendimiento: el tree-diff (saber QUÉ archivos cambiaron) lo hace **gix**,
//! cuyo acceso a objetos es ~20x más rápido que libgit2 en repos grandes. El
//! diff de líneas de cada blob se hace con `git2::Patch::from_buffers` sobre los
//! bytes ya leídos (diff en memoria, sin tocar el object store). Así pasamos de
//! ~460 ms/commit (libgit2 tree-diff) a la velocidad del git CLI.

use crate::FileState;
use std::cell::RefCell;
use std::path::Path;

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

thread_local! {
    /// Repo gix reutilizado entre clics (mismo hilo de UI), con el object-cache
    /// caliente. Evita reabrir el repo y enfriar el cache en cada diff.
    static DIFF_REPO: RefCell<Option<(String, gix::Repository)>> = const { RefCell::new(None) };
}

/// Diff de un commit contra su primer padre (o el árbol vacío si es root).
///
/// Reutiliza un repo gix cacheado por hilo: el primer diff lo abre, los
/// siguientes reaprovechan el cache de objetos caliente. Rápido en repos enormes.
pub fn commit_diff(path: &str, commit_id: &str) -> Result<Vec<FileDiff>, String> {
    DIFF_REPO.with(|cell| {
        let mut slot = cell.borrow_mut();
        let reopen = !matches!(&*slot, Some((p, _)) if p == path);
        if reopen {
            let mut repo = gix::open(path).map_err(|e| e.to_string())?;
            repo.object_cache_size(64 * 1024 * 1024);
            *slot = Some((path.to_string(), repo));
        }
        commit_diff_in(&slot.as_ref().unwrap().1, commit_id)
    })
}

fn commit_diff_in(repo: &gix::Repository, commit_id: &str) -> Result<Vec<FileDiff>, String> {
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

    // Tree-diff con gix: la parte cara, ahora rápida.
    let changes = repo
        .diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)
        .map_err(|e| e.to_string())?;

    let mut files = Vec::with_capacity(changes.len());
    let mut opts = git2::DiffOptions::new();
    opts.context_lines(3);

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

        // Diff de líneas en memoria (sin acceso al object store).
        let p = Path::new(&path_s);
        let patch =
            git2::Patch::from_buffers(&old_bytes, Some(p), &new_bytes, Some(p), Some(&mut opts))
                .map_err(|e| e.to_string())?;
        let (hunks, binary) = patch_to_hunks(patch).map_err(|e| e.to_string())?;

        files.push(FileDiff { path: path_s, old_path, status, binary, hunks });
    }
    Ok(files)
}

/// Diff de los cambios del working tree: sin stage (index vs WT) si `staged=false`,
/// o staged (HEAD vs index) si `staged=true`. Para la vista de Cambios Locales.
pub fn workdir_diff(path: &str, staged: bool) -> Result<Vec<FileDiff>, String> {
    let repo = git2::Repository::open(path).map_err(|e| e.to_string())?;
    let mut opts = git2::DiffOptions::new();
    opts.context_lines(3).include_untracked(true).recurse_untracked_dirs(true);

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

/// Lee los bytes de un blob por su oid (vacío si `None`).
fn read_blob(repo: &gix::Repository, id: Option<gix::ObjectId>) -> Result<Vec<u8>, String> {
    match id {
        Some(id) => Ok(repo.find_object(id).map_err(|e| e.to_string())?.data.clone()),
        None => Ok(Vec::new()),
    }
}

/// Extrae hunks/líneas de un `git2::Patch`. Sin hunks textuales ⇒ binario.
fn patch_to_hunks(patch: git2::Patch) -> Result<(Vec<Hunk>, bool), git2::Error> {
    let n = patch.num_hunks();
    if n == 0 {
        // Puede ser binario o cambio solo de modo. Lo marcamos binario si hay deltas.
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

/// Traduce el `Delta` de libgit2 a nuestro `FileState`.
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
