//! Interactive rebase engine via *replay*: rebuilds history by applying
//! each commit (via tree merge = cherry-pick) onto a new base, following
//! a step plan. Supports pick, reword, squash, fixup, drop and reordering (by
//! the order of the steps). UI-independent.
//!
//! Not the full `git rebase` state machine (no on-disk abort/resume),
//! but it implements the core interactive operations in memory and
//! moves the current branch to the result. If a step conflicts, it aborts without
//! touching the branch (returns `Conflict`).

use crate::Error;

/// Action on a commit in the rebase plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseAction {
    Pick,
    Reword(String),
    Squash,
    Fixup,
    Drop,
}

/// A plan step: which commit and what to do with it.
#[derive(Debug, Clone)]
pub struct RebaseStep {
    pub commit: String,
    pub action: RebaseAction,
}

/// Result of a rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseResult {
    /// Finished; new hash of the branch tip.
    Done(String),
    /// Conflict applying this commit; the branch was not touched.
    Conflict(String),
}

/// Runs an interactive rebase plan onto `base`, rewriting the current branch.
pub(crate) fn run_interactive(
    repo: &git2::Repository,
    base: &str,
    steps: &[RebaseStep],
) -> Result<RebaseResult, Error> {
    let sig = repo.signature()?;
    let mut head = repo.find_commit(git2::Oid::from_str(base)?)?;

    for step in steps {
        if step.action == RebaseAction::Drop {
            continue;
        }
        let commit = repo.find_commit(git2::Oid::from_str(&step.commit)?)?;
        let parent = commit.parent(0)?;

        // Cherry-pick via tree merge: ancestor=commit's parent,
        // "ours"=rebuilt head, "theirs"=the commit to apply.
        let mut idx = repo.merge_trees(&parent.tree()?, &head.tree()?, &commit.tree()?, None)?;
        if idx.has_conflicts() {
            return Ok(RebaseResult::Conflict(step.commit.clone()));
        }
        let tree = repo.find_tree(idx.write_tree_to(repo)?)?;

        head = match &step.action {
            // Squash/Fixup: meld into head (same parent as head, combined tree).
            RebaseAction::Squash | RebaseAction::Fixup => {
                let head_parent = head.parent(0)?;
                let msg = if step.action == RebaseAction::Fixup {
                    head.message().unwrap_or("").trim().to_string()
                } else {
                    format!(
                        "{}\n\n{}",
                        head.message().unwrap_or("").trim(),
                        commit.message().unwrap_or("").trim()
                    )
                };
                let oid = repo.commit(None, &sig, &sig, &msg, &tree, &[&head_parent])?;
                repo.find_commit(oid)?
            }
            RebaseAction::Reword(m) => {
                let oid = repo.commit(None, &sig, &sig, m, &tree, &[&head])?;
                repo.find_commit(oid)?
            }
            // Pick (Drop already filtered above).
            _ => {
                let oid = repo.commit(
                    None,
                    &sig,
                    &sig,
                    commit.message().unwrap_or(""),
                    &tree,
                    &[&head],
                )?;
                repo.find_commit(oid)?
            }
        };
    }

    // Move the current branch to the result and update the working tree.
    let mut head_ref = repo.head()?;
    head_ref.set_target(head.id(), "interactive rebase")?;
    let mut cb = git2::build::CheckoutBuilder::new();
    cb.force();
    repo.checkout_head(Some(&mut cb))?;
    Ok(RebaseResult::Done(head.id().to_string()))
}

/// Non-interactive rebase: replays HEAD's commits (from the merge-base with
/// `upstream`) onto the tip of `upstream`.
pub(crate) fn rebase_onto(repo: &git2::Repository, upstream: &str) -> Result<RebaseResult, Error> {
    let upstream_commit = repo
        .find_branch(upstream, git2::BranchType::Local)?
        .get()
        .peel_to_commit()?;
    let head = repo.head()?.peel_to_commit()?;
    let base = repo.merge_base(head.id(), upstream_commit.id())?;

    let mut walk = repo.revwalk()?;
    walk.push(head.id())?;
    walk.hide(base)?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
    let steps: Vec<RebaseStep> = walk
        .filter_map(|o| o.ok())
        .map(|oid| RebaseStep {
            commit: oid.to_string(),
            action: RebaseAction::Pick,
        })
        .collect();

    run_interactive(repo, &upstream_commit.id().to_string(), &steps)
}
