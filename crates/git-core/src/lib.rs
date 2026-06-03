//! `git-core`: the git layer, independent of the UI.
//!
//! Today it leans on libgit2 (`git2`) for a correct, verifiable baseline
//! from day one. The hot paths (log/diff on huge repos) are
//! migrated to `gitoxide` and benchmarked — but this layer exposes
//! a stable API so the UI (GPUI) never depends on which engine is underneath.

use std::collections::HashMap;
use std::path::Path;

pub mod blame;
pub mod diff;
pub mod graph;
pub mod rebase;
pub mod syntax;

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

/// Kind of ref pointing at a commit (drives the colored chips in the log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Head,
    LocalBranch,
    RemoteBranch,
    Tag,
}

/// A ref label shown as a chip on a commit row.
#[derive(Debug, Clone)]
pub struct RefLabel {
    pub name: String,
    pub kind: RefKind,
}

/// A tag (lightweight or annotated).
#[derive(Debug, Clone)]
pub struct TagInfo {
    pub name: String,
    /// Commit the tag resolves to.
    pub target: String,
    /// Annotation message (empty for lightweight tags).
    pub message: String,
}

/// `reset` mode, mirroring git's `--soft/--mixed/--hard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

/// One entry of the reflog (`HEAD@{n}`).
#[derive(Debug, Clone)]
pub struct ReflogEntry {
    pub id: String,
    pub message: String,
    pub time: i64,
}

/// A configured remote (name + fetch URL).
#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub name: String,
    pub url: String,
}

/// A submodule entry.
#[derive(Debug, Clone)]
pub struct SubmoduleInfo {
    pub name: String,
    pub path: String,
    /// Short hash the submodule is pinned at (if known).
    pub head: Option<String>,
}

/// A stash entry.
#[derive(Debug, Clone)]
pub struct StashInfo {
    pub index: usize,
    pub message: String,
    pub id: String,
}

/// The three sides of a conflicted file, as text (for the 3-way merge viewer).
#[derive(Debug, Clone, Default)]
pub struct ConflictSides {
    pub base: Option<String>,
    pub ours: Option<String>,
    pub theirs: Option<String>,
}

/// How far the current branch is ahead/behind its upstream.
#[derive(Debug, Clone, Copy, Default)]
pub struct AheadBehind {
    pub ahead: usize,
    pub behind: usize,
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

    /// The configured author identity (name, email), if any.
    pub fn user(&self) -> Option<(String, String)> {
        let sig = self.inner.signature().ok()?;
        Some((sig.name()?.to_string(), sig.email()?.to_string()))
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

    /// Detects an in-progress operation that may need abort/continue/skip.
    pub fn op_in_progress(&self) -> Option<String> {
        let gd = self.inner.path();
        if gd.join("rebase-merge").exists() || gd.join("rebase-apply").exists() {
            Some("rebase".into())
        } else if gd.join("MERGE_HEAD").exists() {
            Some("merge".into())
        } else if gd.join("CHERRY_PICK_HEAD").exists() {
            Some("cherry-pick".into())
        } else if gd.join("REVERT_HEAD").exists() {
            Some("revert".into())
        } else {
            None
        }
    }

    pub fn rebase_continue(&self) -> Result<String, String> {
        self.git_cli(&["-c", "core.editor=true", "rebase", "--continue"])
    }
    pub fn rebase_abort(&self) -> Result<String, String> {
        self.git_cli(&["rebase", "--abort"])
    }
    pub fn rebase_skip(&self) -> Result<String, String> {
        self.git_cli(&["rebase", "--skip"])
    }
    pub fn merge_abort(&self) -> Result<String, String> {
        self.git_cli(&["merge", "--abort"])
    }
    pub fn cherry_pick_abort(&self) -> Result<String, String> {
        self.git_cli(&["cherry-pick", "--abort"])
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

    /// Runs an arbitrary `git` subcommand (for the built-in console).
    pub fn git(&self, args: &[&str]) -> Result<String, String> {
        self.git_cli(args)
    }

    /// Writes/refreshes the commit-graph (accelerates log + blame). Cheap to
    /// re-run; best done in the background after opening a repo.
    pub fn write_commit_graph(&self) -> Result<String, String> {
        self.git_cli(&["commit-graph", "write", "--reachable"])
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

impl Repo {
    /// Maps each commit id to the refs (branches/tags/HEAD) pointing at it.
    /// Drives the colored chips drawn on the log rows.
    pub fn refs_by_commit(&self) -> Result<HashMap<String, Vec<RefLabel>>, Error> {
        let mut map: HashMap<String, Vec<RefLabel>> = HashMap::new();
        if let Ok(head) = self.inner.head() {
            if let Ok(commit) = head.peel_to_commit() {
                let name = if head.is_branch() {
                    head.shorthand().unwrap_or("HEAD").to_string()
                } else {
                    "HEAD".to_string()
                };
                map.entry(commit.id().to_string())
                    .or_default()
                    .push(RefLabel { name, kind: RefKind::Head });
            }
        }
        for r in self.inner.references()? {
            let r = match r {
                Ok(r) => r,
                Err(_) => continue,
            };
            let kind = if r.is_branch() {
                RefKind::LocalBranch
            } else if r.is_remote() {
                RefKind::RemoteBranch
            } else if r.is_tag() {
                RefKind::Tag
            } else {
                continue;
            };
            let name = r.shorthand().unwrap_or("").to_string();
            if name.is_empty() || name == "HEAD" {
                continue;
            }
            if let Ok(commit) = r.peel_to_commit() {
                let entry = map.entry(commit.id().to_string()).or_default();
                // Don't duplicate the HEAD branch's chip.
                let dup = kind == RefKind::LocalBranch
                    && entry.iter().any(|x| x.kind == RefKind::Head && x.name == name);
                if !dup {
                    entry.push(RefLabel { name, kind });
                }
            }
        }
        Ok(map)
    }

    // ---- Tags ----
    pub fn tags(&self) -> Result<Vec<TagInfo>, Error> {
        let mut out = Vec::new();
        for name in self.inner.tag_names(None)?.iter().flatten() {
            let full = format!("refs/tags/{name}");
            if let Ok(reference) = self.inner.find_reference(&full) {
                let target = reference
                    .peel_to_commit()
                    .map(|c| c.id().to_string())
                    .unwrap_or_default();
                let message = reference
                    .peel(git2::ObjectType::Tag)
                    .ok()
                    .and_then(|o| o.into_tag().ok())
                    .and_then(|t| t.message().map(|m| m.trim().to_string()))
                    .unwrap_or_default();
                out.push(TagInfo { name: name.to_string(), target, message });
            }
        }
        Ok(out)
    }

    pub fn create_tag(&self, name: &str, target: &str, message: Option<&str>) -> Result<(), Error> {
        let obj = self.inner.find_object(git2::Oid::from_str(target)?, None)?;
        match message {
            Some(m) if !m.trim().is_empty() => {
                let sig = self.inner.signature()?;
                self.inner.tag(name, &obj, &sig, m, false)?;
            }
            _ => {
                self.inner.tag_lightweight(name, &obj, false)?;
            }
        }
        Ok(())
    }

    pub fn delete_tag(&self, name: &str) -> Result<(), Error> {
        self.inner.tag_delete(name)
    }

    pub fn push_tag(&self, remote: &str, name: &str) -> Result<String, String> {
        self.git_cli(&["push", remote, &format!("refs/tags/{name}")])
    }

    // ---- Reset / undo ----
    pub fn reset(&self, target: &str, mode: ResetMode) -> Result<(), Error> {
        let obj = self.inner.find_object(git2::Oid::from_str(target)?, None)?;
        let kind = match mode {
            ResetMode::Soft => git2::ResetType::Soft,
            ResetMode::Mixed => git2::ResetType::Mixed,
            ResetMode::Hard => git2::ResetType::Hard,
        };
        let mut cb = git2::build::CheckoutBuilder::new();
        let checkout = if matches!(mode, ResetMode::Hard) { Some(&mut cb) } else { None };
        self.inner.reset(&obj, kind, checkout)
    }

    /// Undo the last commit, keeping its changes staged (`reset --soft HEAD~1`).
    pub fn uncommit(&self) -> Result<(), Error> {
        let head = self.inner.head()?.peel_to_commit()?;
        let parent = head.parent(0)?;
        self.inner.reset(parent.as_object(), git2::ResetType::Soft, None)
    }

    // ---- Branch (advanced) ----
    pub fn create_branch_at(&self, name: &str, commit_id: &str) -> Result<(), Error> {
        let commit = self.inner.find_commit(git2::Oid::from_str(commit_id)?)?;
        self.inner.branch(name, &commit, false)?;
        Ok(())
    }

    pub fn rename_branch(&self, old: &str, new: &str) -> Result<(), Error> {
        let mut b = self.inner.find_branch(old, git2::BranchType::Local)?;
        b.rename(new, false)?;
        Ok(())
    }

    pub fn set_upstream(&self, branch: &str, upstream: Option<&str>) -> Result<(), Error> {
        let mut b = self.inner.find_branch(branch, git2::BranchType::Local)?;
        b.set_upstream(upstream)
    }

    /// Detached checkout of an arbitrary commit/revision.
    pub fn checkout_commit(&self, id: &str) -> Result<(), Error> {
        let oid = git2::Oid::from_str(id)?;
        let obj = self.inner.find_object(oid, None)?;
        self.inner.checkout_tree(&obj, None)?;
        self.inner.set_head_detached(oid)?;
        Ok(())
    }

    /// Ahead/behind of HEAD vs its configured upstream.
    pub fn ahead_behind(&self) -> Result<AheadBehind, Error> {
        let head = self.inner.head()?;
        let local = head.peel_to_commit()?.id();
        let branch = self
            .inner
            .find_branch(head.shorthand().unwrap_or(""), git2::BranchType::Local)?;
        let upstream = branch.upstream()?.get().peel_to_commit()?.id();
        let (ahead, behind) = self.inner.graph_ahead_behind(local, upstream)?;
        Ok(AheadBehind { ahead, behind })
    }

    // ---- Reflog ----
    pub fn reflog(&self, limit: usize) -> Result<Vec<ReflogEntry>, Error> {
        let reflog = self.inner.reflog("HEAD")?;
        let mut out = Vec::new();
        for entry in reflog.iter().take(limit) {
            out.push(ReflogEntry {
                id: entry.id_new().to_string(),
                message: entry.message().unwrap_or("").to_string(),
                time: entry.committer().when().seconds(),
            });
        }
        Ok(out)
    }

    // ---- Submodules ----
    pub fn submodules(&self) -> Result<Vec<SubmoduleInfo>, Error> {
        let mut out = Vec::new();
        for sm in self.inner.submodules()? {
            out.push(SubmoduleInfo {
                name: sm.name().unwrap_or("").to_string(),
                path: sm.path().to_string_lossy().into_owned(),
                head: sm.workdir_id().or_else(|| sm.head_id()).map(|o| o.to_string()),
            });
        }
        Ok(out)
    }

    // ---- Stash (complete) ----
    pub fn stash_entries(&mut self) -> Result<Vec<StashInfo>, Error> {
        let mut out = Vec::new();
        self.inner.stash_foreach(|idx, msg, oid| {
            out.push(StashInfo { index: idx, message: msg.to_string(), id: oid.to_string() });
            true
        })?;
        Ok(out)
    }

    pub fn stash_apply_index(&mut self, index: usize) -> Result<(), Error> {
        self.inner.stash_apply(index, None)
    }

    pub fn stash_pop_index(&mut self, index: usize) -> Result<(), Error> {
        self.inner.stash_pop(index, None)
    }

    pub fn stash_drop_index(&mut self, index: usize) -> Result<(), Error> {
        self.inner.stash_drop(index)
    }

    // ---- Conflicts (3-way) ----
    /// Reads the three sides of a conflicted file as text.
    pub fn conflict_sides(&self, path: &str) -> Result<ConflictSides, Error> {
        let index = self.inner.index()?;
        let mut sides = ConflictSides::default();
        let read = |entry: &Option<git2::IndexEntry>| -> Option<String> {
            let entry = entry.as_ref()?;
            if String::from_utf8_lossy(&entry.path) != path {
                return None;
            }
            let blob = self.inner.find_blob(entry.id).ok()?;
            Some(String::from_utf8_lossy(blob.content()).into_owned())
        };
        if let Ok(conflicts) = index.conflicts() {
            for c in conflicts.flatten() {
                if let Some(s) = read(&c.ancestor) {
                    sides.base = Some(s);
                }
                if let Some(s) = read(&c.our) {
                    sides.ours = Some(s);
                }
                if let Some(s) = read(&c.their) {
                    sides.theirs = Some(s);
                }
            }
        }
        Ok(sides)
    }

    /// Resolves a conflicted file by taking one side, then stages it.
    pub fn resolve_conflict(&self, path: &str, take_ours: bool) -> Result<String, String> {
        let side = if take_ours { "--ours" } else { "--theirs" };
        self.git_cli(&["checkout", side, "--", path])?;
        self.git_cli(&["add", "--", path])
    }

    // ---- Remotes (management) ----
    pub fn remotes_detailed(&self) -> Result<Vec<RemoteInfo>, Error> {
        let mut out = Vec::new();
        for name in self.inner.remotes()?.iter().flatten() {
            let url = self
                .inner
                .find_remote(name)
                .ok()
                .and_then(|r| r.url().map(str::to_string))
                .unwrap_or_default();
            out.push(RemoteInfo { name: name.to_string(), url });
        }
        Ok(out)
    }

    pub fn add_remote(&self, name: &str, url: &str) -> Result<(), Error> {
        self.inner.remote(name, url)?;
        Ok(())
    }

    pub fn remove_remote(&self, name: &str) -> Result<(), Error> {
        self.inner.remote_delete(name)
    }

    pub fn rename_remote(&self, old: &str, new: &str) -> Result<(), Error> {
        self.inner.remote_rename(old, new)?;
        Ok(())
    }

    pub fn set_remote_url(&self, name: &str, url: &str) -> Result<(), Error> {
        self.inner.remote_set_url(name, url)
    }

    // ---- Remote ops (advanced; via git CLI to reuse the user's credentials) ----
    pub fn fetch_all(&self, prune: bool) -> Result<String, String> {
        if prune {
            self.git_cli(&["fetch", "--all", "--prune", "--tags"])
        } else {
            self.git_cli(&["fetch", "--all", "--tags"])
        }
    }

    pub fn pull_rebase(&self) -> Result<String, String> {
        self.git_cli(&["pull", "--rebase"])
    }

    pub fn pull_merge(&self) -> Result<String, String> {
        self.git_cli(&["pull", "--no-rebase"])
    }

    /// Push with options: force-with-lease, set upstream, push tags.
    pub fn push_opts(
        &self,
        remote: &str,
        branch: &str,
        force_lease: bool,
        set_upstream: bool,
        tags: bool,
    ) -> Result<String, String> {
        let mut args: Vec<String> = vec!["push".into()];
        if force_lease {
            args.push("--force-with-lease".into());
        }
        if set_upstream {
            args.push("--set-upstream".into());
        }
        if tags {
            args.push("--tags".into());
        }
        args.push(remote.to_string());
        args.push(branch.to_string());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.git_cli(&refs)
    }

    // ---- Partial / hunk staging ----
    /// Applies a single file patch to the index (stage a hunk), or reverses it
    /// (unstage a hunk).
    pub fn apply_hunk_to_index(&self, file_patch: &str, reverse: bool) -> Result<(), String> {
        let mut args = vec!["apply", "--cached", "--whitespace=nowarn"];
        if reverse {
            args.push("--reverse");
        }
        args.push("-");
        self.git_cli_stdin(&args, file_patch).map(|_| ())
    }

    /// Applies a single file patch to the working tree. With `reverse`, discards
    /// the hunk (like `git checkout -p` / `git apply --reverse`).
    pub fn apply_hunk_to_worktree(&self, file_patch: &str, reverse: bool) -> Result<(), String> {
        let mut args = vec!["apply", "--whitespace=nowarn"];
        if reverse {
            args.push("--reverse");
        }
        args.push("-");
        self.git_cli_stdin(&args, file_patch).map(|_| ())
    }

    /// Runs `git <args>` feeding `input` on stdin. For patch application.
    fn git_cli_stdin(&self, args: &[&str], input: &str) -> Result<String, String> {
        use std::io::Write;
        let wd = self
            .inner
            .workdir()
            .ok_or_else(|| "repo without working dir".to_string())?;
        let mut child = std::process::Command::new("git")
            .arg("-C")
            .arg(wd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        child
            .stdin
            .as_mut()
            .ok_or("no stdin")?
            .write_all(input.as_bytes())
            .map_err(|e| e.to_string())?;
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }
}

/// Runs `git -C <repo_dir> <args>` and returns stdout (free function; the
/// hot-path log uses gix, but history/pickaxe lean on the CLI for `--follow`
/// rename detection and pickaxe, which gix doesn't expose ergonomically).
fn run_git(repo_dir: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Parses `git log` records emitted with our unit-separated format
/// (`%H \x1f %s \x1f %an \x1f %at \x1f %P`), one record per line.
fn parse_log_records(out: &str) -> Vec<CommitInfo> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split('\u{1f}');
            let id = f.next()?.to_string();
            if id.is_empty() {
                return None;
            }
            let summary = f.next().unwrap_or("").to_string();
            let author = f.next().unwrap_or("").to_string();
            let time = f.next().unwrap_or("0").trim().parse().unwrap_or(0);
            let parents = f
                .next()
                .unwrap_or("")
                .split_whitespace()
                .map(str::to_string)
                .collect();
            Some(CommitInfo { id, summary, author, time, parents })
        })
        .collect()
}

const LOG_FMT: &str = "--pretty=tformat:%H%x1f%s%x1f%an%x1f%at%x1f%P";

/// History of a single file: the commits that touched `path`, following
/// renames (`git log --follow`). Newest first.
pub fn file_history(repo_dir: &str, path: &str, limit: usize) -> Result<Vec<CommitInfo>, String> {
    let lim = format!("-n{limit}");
    let out = run_git(repo_dir, &["log", "--follow", &lim, LOG_FMT, "--", path])?;
    Ok(parse_log_records(&out))
}

/// Pickaxe search across history: commits whose diff changes the number of
/// occurrences of `term` (`-S`, literal), or — when `regex` — whose diff
/// matches `term` as a regex (`-G`). Newest first.
pub fn pickaxe(
    repo_dir: &str,
    term: &str,
    regex: bool,
    limit: usize,
) -> Result<Vec<CommitInfo>, String> {
    if term.trim().is_empty() {
        return Ok(Vec::new());
    }
    let lim = format!("-n{limit}");
    let needle = if regex { format!("-G{term}") } else { format!("-S{term}") };
    let out = run_git(repo_dir, &["log", &needle, &lim, LOG_FMT])?;
    Ok(parse_log_records(&out))
}

/// Clones `url` into `dir` (via the `git` CLI, reusing the user's credentials).
pub fn clone_repo(url: &str, dir: &str) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(["clone", url, dir])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(dir.to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Full detail of a pull request (from `gh pr view`).
#[derive(Debug, Clone, Default)]
pub struct PrDetail {
    pub number: String,
    pub title: String,
    pub state: String,
    pub author: String,
    /// Base branch (merge target).
    pub base: String,
    /// Head branch (the PR's source).
    pub head: String,
    /// Head commit oid — required to post line comments.
    pub head_sha: String,
    pub additions: u32,
    pub deletions: u32,
    pub url: String,
    pub body: String,
}

/// A comment on a PR — either an inline review comment (anchored to
/// `path`/`line`/`side`) or a conversation comment (those fields empty/0).
#[derive(Debug, Clone)]
pub struct PrComment {
    pub id: String,
    pub author: String,
    pub body: String,
    pub path: String,
    pub line: u32,
    /// "RIGHT" (new side) or "LEFT" (old side); empty for conversation comments.
    pub side: String,
    pub created_at: String,
}

/// Runs `gh <args>` with the working directory set to `repo_dir` so `gh`
/// auto-detects the repo from its remote (gh has no global `-C` flag).
fn run_gh(repo_dir: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("gh")
        .current_dir(repo_dir)
        .args(args)
        .output()
        .map_err(|e| format!("gh not found: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Lists pull requests via the `gh` CLI (tab-separated: number, title, branch,
/// state, author). Returns an error string if `gh` is missing or not authed.
pub fn gh_pr_list(repo_dir: &str) -> Result<String, String> {
    run_gh(
        repo_dir,
        &[
            "pr",
            "list",
            "--limit",
            "50",
            "--json",
            "number,title,headRefName,state,author",
            "--template",
            "{{range .}}{{.number}}\t{{.title}}\t{{.headRefName}}\t{{.state}}\t{{.author.login}}\n{{end}}",
        ],
    )
}

/// Runs a `gh pr <args>` subcommand in `repo_dir` (checkout, view --web, …).
pub fn gh_pr(repo_dir: &str, args: &[&str]) -> Result<String, String> {
    let mut a = vec!["pr"];
    a.extend_from_slice(args);
    run_gh(repo_dir, &a)
}

/// jq program that flattens a comment object to our unit-separated TSV record.
/// Newlines in the body are encoded as `` (decoded back in Rust) so each
/// comment stays on one output line.
const PR_COMMENT_JQ: &str = r#".[] | [(.id|tostring),(.user.login // ""),(.path // ""),(((.line // .original_line) // 0)|tostring),(.side // ""),(.created_at // ""),(.body|gsub("\t";"  ")|gsub("\r";"")|gsub("\n";""))] | @tsv"#;

fn parse_pr_comments(out: &str) -> Vec<PrComment> {
    out.lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut f = l.split('\t');
            let id = f.next()?.to_string();
            let author = f.next().unwrap_or("").to_string();
            let path = f.next().unwrap_or("").to_string();
            let line = f.next().unwrap_or("0").trim().parse().unwrap_or(0);
            let side = f.next().unwrap_or("").to_string();
            let created_at = f.next().unwrap_or("").to_string();
            let body = f.next().unwrap_or("").replace('\u{1}', "\n");
            Some(PrComment { id, author, body, path, line, side, created_at })
        })
        .collect()
}

/// Full PR detail (scalars + body) in a single `gh` call: the scalars come back
/// as a TSV first line and the (possibly multi-line) body raw right after. This
/// used to be two round-trips; folding them removes one from the critical path.
pub fn gh_pr_detail(repo_dir: &str, number: &str) -> Result<PrDetail, String> {
    let out = run_gh(
        repo_dir,
        &[
            "pr", "view", number,
            "--json", "number,title,state,author,baseRefName,headRefName,headRefOid,additions,deletions,url,body",
            "--jq", r#"([(.number|tostring),.title,.state,(.author.login // ""),.baseRefName,.headRefName,.headRefOid,(.additions|tostring),(.deletions|tostring),.url]|@tsv), .body"#,
        ],
    )?;
    let mut parts = out.splitn(2, '\n');
    let line = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("").trim_end().to_string();
    let mut f = line.split('\t');
    let mut next = || f.next().unwrap_or("").to_string();
    Ok(PrDetail {
        number: next(),
        title: next(),
        state: next(),
        author: next(),
        base: next(),
        head: next(),
        head_sha: next(),
        additions: next().parse().unwrap_or(0),
        deletions: next().parse().unwrap_or(0),
        url: next(),
        body,
    })
}

/// jq that synthesizes a `git`-style diff from the Files API response
/// (`pulls/{n}/files`): for each file a `diff --git` header + status markers +
/// the `patch` body. Lets us reuse `parse_unified_diff` untouched. `gh pr diff`
/// can't serve PRs with >300 files (HTTP 406), but this endpoint paginates.
const PR_FILES_JQ: &str = r#".[] | "diff --git a/\(.previous_filename // .filename) b/\(.filename)", (if .status=="added" then "new file mode 100644" elif .status=="removed" then "deleted file mode 100644" elif .status=="renamed" then "rename from \(.previous_filename)\nrename to \(.filename)" else empty end), (.patch // empty)"#;

/// The PR's unified diff. Fast path: `gh pr diff` (one request, ideal for normal
/// PRs). GitHub refuses that with HTTP 406 once a PR touches >300 files, so for
/// huge PRs we fall back to the paginated Files API, fetching pages concurrently
/// and stitching them back in order — ~4s instead of ~18s serial for 2000 files.
pub fn gh_pr_diff(repo_dir: &str, number: &str) -> Result<String, String> {
    match run_gh(repo_dir, &["pr", "diff", number]) {
        Ok(diff) => Ok(diff),
        Err(e) if e.contains("maximum number of files") || e.contains("too_large") => {
            gh_pr_diff_from_files(repo_dir, number)
        }
        Err(e) => Err(e),
    }
}

/// Item count from the PR object (`changed_files`, `review_comments`,
/// `comments`, …), used to size parallel pagination. 0 if it can't be read.
fn gh_pr_count(repo_dir: &str, number: &str, field: &str) -> usize {
    run_gh(
        repo_dir,
        &["api", &format!("repos/{{owner}}/{{repo}}/pulls/{number}"), "--jq", &format!(".{field}")],
    )
    .ok()
    .and_then(|s| s.trim().parse().ok())
    .unwrap_or(0)
}

/// One page (100 items) of a `gh api` endpoint, flattened by `jq`.
fn fetch_api_page(repo_dir: &str, endpoint: &str, jq: &str, page: usize) -> Result<String, String> {
    let sep = if endpoint.contains('?') { '&' } else { '?' };
    let ep = format!("{endpoint}{sep}per_page=100&page={page}");
    run_gh(repo_dir, &["api", &ep, "--jq", jq])
}

/// Fetches a paginated `gh api` endpoint with the pages run concurrently and
/// stitched back in order — the project-wide pattern for "fast on huge PRs".
///
/// Page 1 and the item count are fetched *together*, so a single-page result
/// (small PRs, the common case) costs exactly one round-trip — no penalty for
/// going parallel. Only when there's genuinely more than one page do we fan out
/// pages 2..=N in bounded-concurrency waves (each `gh` call is a blocking
/// process, so we cap how many run at once). `jq` flattens each page.
fn gh_api_paginated_parallel(
    repo_dir: &str,
    endpoint: &str,
    number: &str,
    count_field: &str,
    jq: &str,
) -> String {
    let (page1, count) = std::thread::scope(|s| {
        let h_p1 = s.spawn(|| fetch_api_page(repo_dir, endpoint, jq, 1).unwrap_or_default());
        let h_cnt = s.spawn(|| gh_pr_count(repo_dir, number, count_field));
        (h_p1.join().unwrap_or_default(), h_cnt.join().unwrap_or(0))
    });
    let pages = count.div_ceil(100).max(1);
    if pages <= 1 {
        return page1;
    }
    // More than one page: fan out 2..=pages. Index i holds page i+1.
    const CONC: usize = 16;
    let mut out: Vec<String> = vec![String::new(); pages];
    out[0] = page1;
    let mut start = 1;
    while start < pages {
        let end = (start + CONC).min(pages);
        std::thread::scope(|s| {
            let handles: Vec<_> = (start..end)
                .map(|i| s.spawn(move || (i, fetch_api_page(repo_dir, endpoint, jq, i + 1))))
                .collect();
            // Best-effort: a failed page is left empty rather than failing all.
            for h in handles {
                if let Ok((i, Ok(txt))) = h.join() {
                    out[i] = txt;
                }
            }
        });
        start = end;
    }
    out.concat()
}

/// Rebuilds the diff from the Files API when `gh pr diff` bails on a huge PR.
fn gh_pr_diff_from_files(repo_dir: &str, number: &str) -> Result<String, String> {
    let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{number}/files");
    Ok(gh_api_paginated_parallel(repo_dir, &endpoint, number, "changed_files", PR_FILES_JQ))
}

/// Inline review comments (anchored to file lines). Pages fetched concurrently.
pub fn gh_pr_review_comments(repo_dir: &str, number: &str) -> Result<Vec<PrComment>, String> {
    let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{number}/comments");
    let out = gh_api_paginated_parallel(repo_dir, &endpoint, number, "review_comments", PR_COMMENT_JQ);
    Ok(parse_pr_comments(&out))
}

/// Conversation (issue) comments on the PR. Pages fetched concurrently.
pub fn gh_pr_conversation(repo_dir: &str, number: &str) -> Result<Vec<PrComment>, String> {
    let endpoint = format!("repos/{{owner}}/{{repo}}/issues/{number}/comments");
    let out = gh_api_paginated_parallel(repo_dir, &endpoint, number, "comments", PR_COMMENT_JQ);
    Ok(parse_pr_comments(&out))
}

/// Posts a single inline review comment on `path` at `line` (`side` =
/// RIGHT/LEFT) against the PR's head commit.
pub fn gh_pr_add_comment(
    repo_dir: &str,
    number: &str,
    head_sha: &str,
    path: &str,
    line: u32,
    side: &str,
    body: &str,
) -> Result<String, String> {
    let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{number}/comments");
    run_gh(
        repo_dir,
        &[
            "api", "--method", "POST", &endpoint,
            "-f", &format!("body={body}"),
            "-f", &format!("commit_id={head_sha}"),
            "-f", &format!("path={path}"),
            "-F", &format!("line={line}"),
            "-f", &format!("side={side}"),
        ],
    )
    .map(|_| "inline comment posted".into())
}

/// Replies to an existing review-comment thread.
pub fn gh_pr_reply(repo_dir: &str, number: &str, comment_id: &str, body: &str) -> Result<String, String> {
    let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{number}/comments/{comment_id}/replies");
    run_gh(repo_dir, &["api", "--method", "POST", &endpoint, "-f", &format!("body={body}")])
        .map(|_| "reply posted".into())
}

/// Posts a general conversation comment on the PR.
pub fn gh_pr_comment_general(repo_dir: &str, number: &str, body: &str) -> Result<String, String> {
    run_gh(repo_dir, &["pr", "comment", number, "--body", body]).map(|_| "comment posted".into())
}

/// Submits a review: `kind` ∈ {"approve","request-changes","comment"}.
pub fn gh_pr_review(repo_dir: &str, number: &str, kind: &str, body: &str) -> Result<String, String> {
    let flag = match kind {
        "approve" => "--approve",
        "request-changes" => "--request-changes",
        _ => "--comment",
    };
    let mut args = vec!["pr", "review", number, flag];
    if !body.trim().is_empty() {
        args.push("--body");
        args.push(body);
    }
    run_gh(repo_dir, &args).map(|_| format!("review submitted ({kind})"))
}

/// Merges the PR: `method` ∈ {"merge","squash","rebase"}.
pub fn gh_pr_merge(repo_dir: &str, number: &str, method: &str) -> Result<String, String> {
    let flag = match method {
        "squash" => "--squash",
        "rebase" => "--rebase",
        _ => "--merge",
    };
    run_gh(repo_dir, &["pr", "merge", number, flag])
}

/// `log` via **gitoxide (gix)** from an explicit starting commit (hex id).
/// Powers the per-branch log filter ("show only this branch").
pub fn gix_log_from(
    path: &str,
    start_id: &str,
    limit: usize,
) -> Result<Vec<CommitInfo>, Box<dyn std::error::Error>> {
    use gix::revision::walk::Sorting;
    use gix::traverse::commit::simple::CommitTimeOrder;

    let mut repo = gix::open(path)?;
    repo.object_cache_size(32 * 1024 * 1024);
    let oid = gix::ObjectId::from_hex(start_id.as_bytes())?;
    let walk = repo
        .rev_walk(Some(oid))
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .all()?;

    let mut out = Vec::with_capacity(limit.min(1024));
    for info in walk.take(limit) {
        let info = info?;
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

/// Resolves a ref name (branch/tag/HEAD) to a commit and logs from it.
pub fn gix_log_ref(
    path: &str,
    refname: &str,
    limit: usize,
) -> Result<Vec<CommitInfo>, Box<dyn std::error::Error>> {
    let repo = git2::Repository::open(path)?;
    let oid = repo.revparse_single(refname)?.peel_to_commit()?.id();
    gix_log_from(path, &oid.to_string(), limit)
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
