//! `git-core`: the git layer, independent of the UI.
//!
//! Today it leans on libgit2 (`git2`) for a correct, verifiable baseline
//! from day one. The hot paths (log/diff on huge repos) are
//! migrated to `gitoxide` and benchmarked — but this layer exposes
//! a stable API so the UI (GPUI) never depends on which engine is underneath.

use std::path::Path;

pub mod blame;
pub mod diff;
pub mod graph;
pub mod rebase;

pub use git2::Error;

/// An open git repository.
pub struct Repo {
    inner: git2::Repository,
}

/// Minimal commit data for drawing the log/graph.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    /// Full hash (40 hex).
    pub id: String,
    pub summary: String,
    pub author: String,
    /// Commit time in Unix seconds.
    pub time: i64,
    /// Parents (1 = normal, 2+ = merge, 0 = root). Needed for the graph.
    pub parents: Vec<String>,
}

/// State of a file in the working tree or the index.
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

/// A `status` entry: a pending change.
#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub path: String,
    pub state: FileState,
    /// `true` if staged (in the index), `false` if only in the WT.
    pub staged: bool,
}

/// A local branch.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub upstream: Option<String>,
}

/// Result of merging a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    UpToDate,
    FastForward(String),
    Merged(String),
    Conflicts,
}

impl Repo {
    /// Opens the repo containing `path` (discovers `.git` upward).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let inner = git2::Repository::discover(path)?;
        Ok(Self { inner })
    }

    /// The `limit` most recent commits from HEAD, in reverse-chronological order.
    pub fn log(&self, limit: usize) -> Result<Vec<CommitInfo>, Error> {
        let mut walk = self.inner.revwalk()?;
        walk.push_head()?;
        // PERFORMANCE NOTE (measured on a 505k-commit repo, macOS arm64):
        //   - default order (GIT_SORT_NONE): ~369 ms / 1000 commits
        //   - Sort::TIME:                         ~2.97 s / 1000 commits (8x worse)
        //   - git CLI (reference):               ~0.02 s
        // libgit2 is 18-150x slower than git here. That's why sorting and
        // log migrate to `gitoxide` (reads commit-graph, much faster ODB).
        // For now we use the default order; topological/date sorting
        // will use gix with commit-graph generation numbers.

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

    /// Pending changes (working tree + index). The basis of the commit view.
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

    /// Adds a file to the index (stage). Handles modified/new and deleted.
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
            index.remove_path(Path::new(path))?; // deleted file
        }
        index.write()
    }

    /// Removes a file from the index (unstage): restores it to HEAD's state.
    pub fn unstage(&self, path: &str) -> Result<(), Error> {
        match self.inner.head() {
            Ok(head) => {
                let obj = head.peel(git2::ObjectType::Commit)?;
                self.inner.reset_default(Some(&obj), [path])?;
            }
            Err(_) => {
                // Repo with no commits yet: just remove from the index.
                let mut index = self.inner.index()?;
                index.remove_path(Path::new(path))?;
                index.write()?;
            }
        }
        Ok(())
    }

    /// Creates a commit from the index. Returns the hash.
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

    /// Amends the HEAD commit with the current index and a new message.
    pub fn amend(&self, message: &str) -> Result<String, Error> {
        let head = self.inner.head()?.peel_to_commit()?;
        let mut index = self.inner.index()?;
        let tree = self.inner.find_tree(index.write_tree()?)?;
        let oid = head.amend(Some("HEAD"), None, None, None, Some(message), Some(&tree))?;
        Ok(oid.to_string())
    }

    /// Reverts a commit into the working tree + index (like `git revert --no-commit`).
    pub fn revert_commit(&self, id: &str) -> Result<(), Error> {
        let commit = self.inner.find_commit(git2::Oid::from_str(id)?)?;
        self.inner.revert(&commit, None)
    }

    /// Applies a commit (cherry-pick) into the working tree + index.
    pub fn cherry_pick(&self, id: &str) -> Result<(), Error> {
        let commit = self.inner.find_commit(git2::Oid::from_str(id)?)?;
        self.inner.cherrypick(&commit, None)
    }

    /// Discards a file's unstaged changes (rollback to the index state).
    pub fn discard(&self, path: &str) -> Result<(), Error> {
        let mut cb = git2::build::CheckoutBuilder::new();
        cb.path(path).force();
        self.inner.checkout_index(None, Some(&mut cb))
    }

    // ---- Branches ----

    /// Lists local branches (marks the current one).
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

    /// Creates a new branch at HEAD.
    pub fn create_branch(&self, name: &str) -> Result<(), Error> {
        let head = self.inner.head()?.peel_to_commit()?;
        self.inner.branch(name, &head, false)?;
        Ok(())
    }

    /// Switches to branch `name` (safe checkout).
    pub fn checkout_branch(&self, name: &str) -> Result<(), Error> {
        let refname = format!("refs/heads/{name}");
        let obj = self.inner.revparse_single(&refname)?;
        self.inner.checkout_tree(&obj, None)?;
        self.inner.set_head(&refname)?;
        Ok(())
    }

    /// Deletes a local branch.
    pub fn delete_branch(&self, name: &str) -> Result<(), Error> {
        self.inner
            .find_branch(name, git2::BranchType::Local)?
            .delete()
    }

    /// Merges `name` into the current branch. Handles fast-forward and normal merge.
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

    /// Lists the repo's worktrees.
    pub fn worktrees(&self) -> Result<Vec<String>, Error> {
        Ok(self
            .inner
            .worktrees()?
            .iter()
            .flatten()
            .map(str::to_string)
            .collect())
    }

    // ---- Rebase 🎯 ----

    /// Runs an interactive rebase plan onto `base` (pick/reword/squash/fixup/drop).
    pub fn rebase_interactive(
        &self,
        base: &str,
        steps: &[rebase::RebaseStep],
    ) -> Result<rebase::RebaseResult, Error> {
        rebase::run_interactive(&self.inner, base, steps)
    }

    /// Rebases the current branch onto the tip of `upstream`.
    pub fn rebase_onto(&self, upstream: &str) -> Result<rebase::RebaseResult, Error> {
        rebase::rebase_onto(&self.inner, upstream)
    }

    // ---- Remote ----

    /// Lists the configured remotes.
    pub fn remotes(&self) -> Result<Vec<String>, Error> {
        Ok(self.inner.remotes()?.iter().flatten().map(str::to_string).collect())
    }

    /// Runs `git <args>` in the repo's working dir. Used for
    /// NETWORK operations: reuses the user's credentials/SSH without
    /// reimplementing auth or compiling openssl. The network is I/O, no perf hit.
    fn git_cli(&self, args: &[&str]) -> Result<String, String> {
        let wd = self
            .inner
            .workdir()
            .ok_or_else(|| "repo sin working dir".to_string())?;
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(wd)
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    /// fetch from the given remote.
    pub fn fetch(&self, remote: &str) -> Result<String, String> {
        self.git_cli(&["fetch", remote])
    }

    /// fast-forward pull of the current branch.
    pub fn pull(&self) -> Result<String, String> {
        self.git_cli(&["pull", "--ff-only"])
    }

    /// push `branch` to `remote`.
    pub fn push(&self, remote: &str, branch: &str) -> Result<String, String> {
        self.git_cli(&["push", remote, branch])
    }

    // ---- Other: stash, ignore, conflicts ----

    /// Saves the current changes to a stash. Returns the stash hash.
    pub fn stash_save(&mut self, message: &str) -> Result<String, Error> {
        let sig = self.inner.signature()?;
        let oid = self.inner.stash_save(&sig, message, None)?;
        Ok(oid.to_string())
    }

    /// Lists the stashes (`stash@{i}: message`).
    pub fn stash_list(&mut self) -> Result<Vec<String>, Error> {
        let mut out = Vec::new();
        self.inner.stash_foreach(|idx, msg, _oid| {
            out.push(format!("stash@{{{idx}}}: {msg}"));
            true
        })?;
        Ok(out)
    }

    /// Applies and pops the most recent stash.
    pub fn stash_pop(&mut self) -> Result<(), Error> {
        self.inner.stash_pop(0, None)
    }

    /// Appends a pattern to the repo's `.gitignore`.
    pub fn add_to_gitignore(&self, pattern: &str) -> Result<(), Error> {
        use std::io::Write;
        let wd = self
            .inner
            .workdir()
            .ok_or_else(|| Error::from_str("repo sin working dir"))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(wd.join(".gitignore"))
            .map_err(|e| Error::from_str(&e.to_string()))?;
        writeln!(f, "{pattern}").map_err(|e| Error::from_str(&e.to_string()))?;
        Ok(())
    }

    /// Is the path ignored by gitignore?
    pub fn is_ignored(&self, path: &str) -> Result<bool, Error> {
        self.inner.is_path_ignored(path)
    }

    /// Conflicted files (basis for the 3-way merge viewer).
    pub fn conflicts(&self) -> Result<Vec<String>, Error> {
        let index = self.inner.index()?;
        let mut out = Vec::new();
        if let Ok(conflicts) = index.conflicts() {
            for c in conflicts.flatten() {
                if let Some(entry) = c.our.or(c.their).or(c.ancestor) {
                    out.push(String::from_utf8_lossy(&entry.path).into_owned());
                }
            }
        }
        Ok(out)
    }
}

/// `log` via **gitoxide (gix)** — the candidate engine for the hot paths.
/// Same contract as [`Repo::log`] but with a much faster ODB. Returns
/// the `limit` most recent commits ordered by commit date (newest first).
pub fn gix_log(path: &str, limit: usize) -> Result<Vec<CommitInfo>, Box<dyn std::error::Error>> {
    use gix::revision::walk::Sorting;
    use gix::traverse::commit::simple::CommitTimeOrder;

    let mut repo = gix::open(path)?;
    // Object cache: gix warns the date walk looks up each commit
    // twice (sort, then read author/summary); the cache avoids it.
    repo.object_cache_size(32 * 1024 * 1024);

    let head = repo.head_id()?;
    let walk = repo
        .rev_walk(Some(head.detach()))
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .all()?;

    let mut out = Vec::with_capacity(limit.min(1024));
    for info in walk.take(limit) {
        let info = info?;
        // parents and time come free from Info; we only decode for author+summary.
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

/// Translates libgit2 flags to our `FileState`. Conflict wins; then
/// we prioritize what's staged (index) over the working tree.
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
