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
pub mod rebase;

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

/// Una rama local.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub upstream: Option<String>,
}

/// Resultado de fusionar una rama.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    UpToDate,
    FastForward(String),
    Merged(String),
    Conflicts,
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

    /// Enmienda (amend) el commit HEAD con el index actual y un nuevo mensaje.
    pub fn amend(&self, message: &str) -> Result<String, Error> {
        let head = self.inner.head()?.peel_to_commit()?;
        let mut index = self.inner.index()?;
        let tree = self.inner.find_tree(index.write_tree()?)?;
        let oid = head.amend(Some("HEAD"), None, None, None, Some(message), Some(&tree))?;
        Ok(oid.to_string())
    }

    /// Revierte un commit en el working tree + index (como `git revert --no-commit`).
    pub fn revert_commit(&self, id: &str) -> Result<(), Error> {
        let commit = self.inner.find_commit(git2::Oid::from_str(id)?)?;
        self.inner.revert(&commit, None)
    }

    /// Aplica un commit (cherry-pick) en el working tree + index.
    pub fn cherry_pick(&self, id: &str) -> Result<(), Error> {
        let commit = self.inner.find_commit(git2::Oid::from_str(id)?)?;
        self.inner.cherrypick(&commit, None)
    }

    /// Descarta los cambios no-staged de un archivo (rollback al estado del index).
    pub fn discard(&self, path: &str) -> Result<(), Error> {
        let mut cb = git2::build::CheckoutBuilder::new();
        cb.path(path).force();
        self.inner.checkout_index(None, Some(&mut cb))
    }

    // ---- Ramas (área Ramas) ----

    /// Lista las ramas locales (marca la actual).
    pub fn branches(&self) -> Result<Vec<BranchInfo>, Error> {
        let mut out = Vec::new();
        for b in self.inner.branches(Some(git2::BranchType::Local))? {
            let (branch, _) = b?;
            let is_head = branch.is_head();
            let name = branch.name()?.unwrap_or("").to_string();
            let upstream = branch
                .upstream()
                .ok()
                .and_then(|u| u.name().ok().flatten().map(str::to_string));
            out.push(BranchInfo { name, is_head, upstream });
        }
        Ok(out)
    }

    /// Crea una rama nueva en HEAD.
    pub fn create_branch(&self, name: &str) -> Result<(), Error> {
        let head = self.inner.head()?.peel_to_commit()?;
        self.inner.branch(name, &head, false)?;
        Ok(())
    }

    /// Cambia a la rama `name` (checkout seguro).
    pub fn checkout_branch(&self, name: &str) -> Result<(), Error> {
        let refname = format!("refs/heads/{name}");
        let obj = self.inner.revparse_single(&refname)?;
        self.inner.checkout_tree(&obj, None)?;
        self.inner.set_head(&refname)?;
        Ok(())
    }

    /// Borra una rama local.
    pub fn delete_branch(&self, name: &str) -> Result<(), Error> {
        self.inner
            .find_branch(name, git2::BranchType::Local)?
            .delete()
    }

    /// Fusiona `name` en la rama actual. Maneja fast-forward y merge normal.
    pub fn merge_branch(&self, name: &str) -> Result<MergeOutcome, Error> {
        let their_commit = self
            .inner
            .find_branch(name, git2::BranchType::Local)?
            .get()
            .peel_to_commit()?;
        let annotated = self.inner.find_annotated_commit(their_commit.id())?;
        let (analysis, _) = self.inner.merge_analysis(&[&annotated])?;

        if analysis.is_up_to_date() {
            return Ok(MergeOutcome::UpToDate);
        }
        if analysis.is_fast_forward() {
            let mut head_ref = self.inner.head()?;
            head_ref.set_target(their_commit.id(), "fast-forward")?;
            self.inner.set_head(head_ref.name().unwrap_or("HEAD"))?;
            let mut cb = git2::build::CheckoutBuilder::new();
            cb.force();
            self.inner.checkout_head(Some(&mut cb))?;
            return Ok(MergeOutcome::FastForward(their_commit.id().to_string()));
        }

        self.inner.merge(&[&annotated], None, None)?;
        if self.inner.index()?.has_conflicts() {
            return Ok(MergeOutcome::Conflicts);
        }
        let tree = self.inner.find_tree(self.inner.index()?.write_tree()?)?;
        let sig = self.inner.signature()?;
        let head_commit = self.inner.head()?.peel_to_commit()?;
        let oid = self.inner.commit(
            Some("HEAD"),
            &sig,
            &sig,
            &format!("Merge branch '{name}'"),
            &tree,
            &[&head_commit, &their_commit],
        )?;
        self.inner.cleanup_state()?;
        Ok(MergeOutcome::Merged(oid.to_string()))
    }

    /// Lista los worktrees del repo.
    pub fn worktrees(&self) -> Result<Vec<String>, Error> {
        Ok(self
            .inner
            .worktrees()?
            .iter()
            .flatten()
            .map(str::to_string)
            .collect())
    }

    // ---- Rebase (área Rebase 🎯) ----

    /// Ejecuta un plan de rebase interactivo sobre `base` (pick/reword/squash/fixup/drop).
    pub fn rebase_interactive(
        &self,
        base: &str,
        steps: &[rebase::RebaseStep],
    ) -> Result<rebase::RebaseResult, Error> {
        rebase::run_interactive(&self.inner, base, steps)
    }

    /// Rebase de la rama actual sobre la punta de `upstream`.
    pub fn rebase_onto(&self, upstream: &str) -> Result<rebase::RebaseResult, Error> {
        rebase::rebase_onto(&self.inner, upstream)
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
