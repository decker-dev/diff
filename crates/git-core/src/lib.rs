//! `git-core`: la capa de git, independiente de la UI.
//!
//! Hoy se apoya en libgit2 (`git2`) para tener una base correcta y verificable
//! desde el día uno. Las rutas calientes (log/diff sobre repos enormes) se
//! migrarán a `gitoxide` y se compararán por benchmark — pero esta capa expone
//! una API estable para que la UI (GPUI) nunca dependa de qué motor hay debajo.

use std::path::Path;

pub mod blame;
pub mod diff;
pub mod graph;

pub use git2::Error;

/// Un repositorio git abierto.
pub struct Repo {
    inner: git2::Repository,
}

/// Datos mínimos de un commit para pintar el log/graph.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    /// Hash completo (40 hex).
    pub id: String,
    pub summary: String,
    pub author: String,
    /// Tiempo del commit en segundos Unix.
    pub time: i64,
    /// Padres (1 = normal, 2+ = merge, 0 = root). Necesario para el graph.
    pub parents: Vec<String>,
}

/// Estado de un archivo en el working tree o el index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    New,
    Modified,
    Deleted,
    Renamed,
    TypeChange,
    Conflicted,
    Untracked,
}

/// Una entrada de `status`: un cambio pendiente.
#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub path: String,
    pub state: FileState,
    /// `true` si el cambio está en el index (staged), `false` si solo en el WT.
    pub staged: bool,
}

impl Repo {
    /// Abre el repo que contiene `path` (descubre el `.git` hacia arriba).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let inner = git2::Repository::discover(path)?;
        Ok(Self { inner })
    }

    /// Los `limit` commits más recientes desde HEAD, en orden cronológico inverso.
    pub fn log(&self, limit: usize) -> Result<Vec<CommitInfo>, Error> {
        let mut walk = self.inner.revwalk()?;
        walk.push_head()?;
        // NOTA DE RENDIMIENTO (medido en repo de 505k commits, macOS arm64):
        //   - orden por defecto (GIT_SORT_NONE): ~369 ms / 1000 commits
        //   - Sort::TIME:                         ~2,97 s / 1000 commits (8x peor)
        //   - git CLI (referencia):               ~0,02 s
        // libgit2 es 18-150x más lento que git aquí. Por eso el ordenado y el
        // log van a migrar a `gitoxide` (lee commit-graph, ODB mucho más rápido).
        // De momento usamos el orden por defecto; el ordenado topológico/fecha
        // se hará sobre gix con números de generación del commit-graph.

        let mut out = Vec::with_capacity(limit.min(1024));
        for oid in walk.take(limit) {
            let oid = oid?;
            let commit = self.inner.find_commit(oid)?;
            out.push(CommitInfo {
                id: oid.to_string(),
                summary: commit.summary().unwrap_or("").to_string(),
                author: commit.author().name().unwrap_or("?").to_string(),
                time: commit.time().seconds(),
                parents: commit.parent_ids().map(|p| p.to_string()).collect(),
            });
        }
        Ok(out)
    }

    /// Cambios pendientes (working tree + index). La base de la vista de commit.
    pub fn status(&self) -> Result<Vec<StatusEntry>, Error> {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(false)
            .include_ignored(false);

        let statuses = self.inner.statuses(Some(&mut opts))?;
        let mut out = Vec::with_capacity(statuses.len());
        for entry in statuses.iter() {
            let (state, staged) = classify(entry.status());
            out.push(StatusEntry {
                path: entry.path().unwrap_or("").to_string(),
                state,
                staged,
            });
        }
        Ok(out)
    }

    /// Añade un archivo al index (stage). Maneja modificados/nuevos y borrados.
    pub fn stage(&self, path: &str) -> Result<(), Error> {
        let mut index = self.inner.index()?;
        let exists = self
            .inner
            .workdir()
            .map(|w| w.join(path).exists())
            .unwrap_or(false);
        if exists {
            index.add_path(Path::new(path))?;
        } else {
            index.remove_path(Path::new(path))?; // archivo borrado
        }
        index.write()
    }

    /// Quita un archivo del index (unstage): lo devuelve al estado de HEAD.
    pub fn unstage(&self, path: &str) -> Result<(), Error> {
        match self.inner.head() {
            Ok(head) => {
                let obj = head.peel(git2::ObjectType::Commit)?;
                self.inner.reset_default(Some(&obj), [path])?;
            }
            Err(_) => {
                // Repo sin commits aún: simplemente quitar del index.
                let mut index = self.inner.index()?;
                index.remove_path(Path::new(path))?;
                index.write()?;
            }
        }
        Ok(())
    }

    /// Crea un commit con lo que haya en el index. Devuelve el hash.
    pub fn commit(&self, message: &str) -> Result<String, Error> {
        let sig = self.inner.signature()?;
        let mut index = self.inner.index()?;
        let tree = self.inner.find_tree(index.write_tree()?)?;
        let oid = match self.inner.head().and_then(|h| h.peel_to_commit()) {
            Ok(parent) => {
                self.inner
                    .commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])?
            }
            Err(_) => self.inner.commit(Some("HEAD"), &sig, &sig, message, &tree, &[])?,
        };
        Ok(oid.to_string())
    }
}

/// `log` vía **gitoxide (gix)** — el motor candidato para las rutas calientes.
/// Mismo contrato que [`Repo::log`] pero con un ODB mucho más rápido. Devuelve
/// los `limit` commits más recientes ordenados por fecha de commit (nuevos primero).
pub fn gix_log(path: &str, limit: usize) -> Result<Vec<CommitInfo>, Box<dyn std::error::Error>> {
    use gix::revision::walk::Sorting;
    use gix::traverse::commit::simple::CommitTimeOrder;

    let mut repo = gix::open(path)?;
    // Cache de objetos: gix avisa que el walk por fecha consulta cada commit dos
    // veces (una para ordenar, otra para leer autor/summary); el cache lo evita.
    repo.object_cache_size(32 * 1024 * 1024);

    let head = repo.head_id()?;
    let walk = repo
        .rev_walk(Some(head.detach()))
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .all()?;

    let mut out = Vec::with_capacity(limit.min(1024));
    for info in walk.take(limit) {
        let info = info?;
        // parents y time salen gratis del Info; solo decodificamos para autor+summary.
        let commit = repo.find_object(info.id)?.try_into_commit()?;
        let author = commit.author()?;
        let message = commit.message()?;
        out.push(CommitInfo {
            id: info.id.to_string(),
            summary: message.summary().to_string(),
            author: author.name.to_string(),
            time: info.commit_time.unwrap_or(0),
            parents: info.parent_ids.iter().map(|id| id.to_string()).collect(),
        });
    }
    Ok(out)
}

/// Traduce los flags de libgit2 a nuestro `FileState`. Conflicto manda; luego
/// damos prioridad a lo que está en el index (staged) sobre el working tree.
fn classify(s: git2::Status) -> (FileState, bool) {
    use git2::Status as S;
    if s.contains(S::CONFLICTED) {
        return (FileState::Conflicted, false);
    }
    if s.contains(S::INDEX_NEW) {
        return (FileState::New, true);
    }
    if s.contains(S::INDEX_MODIFIED) {
        return (FileState::Modified, true);
    }
    if s.contains(S::INDEX_DELETED) {
        return (FileState::Deleted, true);
    }
    if s.contains(S::INDEX_RENAMED) {
        return (FileState::Renamed, true);
    }
    if s.contains(S::INDEX_TYPECHANGE) {
        return (FileState::TypeChange, true);
    }
    if s.contains(S::WT_NEW) {
        return (FileState::Untracked, false);
    }
    if s.contains(S::WT_DELETED) {
        return (FileState::Deleted, false);
    }
    if s.contains(S::WT_RENAMED) {
        return (FileState::Renamed, false);
    }
    if s.contains(S::WT_TYPECHANGE) {
        return (FileState::TypeChange, false);
    }
    (FileState::Modified, false)
}
