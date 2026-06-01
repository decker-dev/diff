//! Motor de rebase interactivo por *replay*: reconstruye la historia aplicando
//! cada commit (vía merge de árboles = cherry-pick) sobre una base nueva, según
//! un plan de pasos. Soporta pick, reword, squash, fixup, drop y reordenado (por
//! el orden de los pasos). Independiente de la UI.
//!
//! No es la máquina de estados completa de `git rebase` (no hay abort/resume en
//! disco), pero implementa las operaciones interactivas centrales en memoria y
//! mueve la rama actual al resultado. Si un paso genera conflicto, se aborta sin
//! tocar la rama (devuelve `Conflict`).

use crate::Error;

/// Acción sobre un commit en el plan de rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseAction {
    Pick,
    Reword(String),
    Squash,
    Fixup,
    Drop,
}

/// Un paso del plan: qué commit y qué hacer con él.
#[derive(Debug, Clone)]
pub struct RebaseStep {
    pub commit: String,
    pub action: RebaseAction,
}

/// Resultado de un rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseResult {
    /// Terminó; nuevo hash de la punta de la rama.
    Done(String),
    /// Conflicto al aplicar este commit; la rama no se tocó.
    Conflict(String),
}

/// Ejecuta un plan de rebase interactivo sobre `base`, reescribiendo la rama actual.
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

        // Cherry-pick por merge de árboles: ancestro=padre del commit,
        // "nuestro"=head reconstruido, "suyo"=el commit a aplicar.
        let mut idx = repo.merge_trees(&parent.tree()?, &head.tree()?, &commit.tree()?, None)?;
        if idx.has_conflicts() {
            return Ok(RebaseResult::Conflict(step.commit.clone()));
        }
        let tree = repo.find_tree(idx.write_tree_to(repo)?)?;

        head = match &step.action {
            // Squash/Fixup: fundir en head (mismo padre que head, árbol combinado).
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
            // Pick (y Drop ya filtrado arriba).
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

    // Mover la rama actual al resultado y actualizar el working tree.
    let mut head_ref = repo.head()?;
    head_ref.set_target(head.id(), "rebase interactivo")?;
    let mut cb = git2::build::CheckoutBuilder::new();
    cb.force();
    repo.checkout_head(Some(&mut cb))?;
    Ok(RebaseResult::Done(head.id().to_string()))
}

/// Rebase no interactivo: reaplica los commits de HEAD (desde la merge-base con
/// `upstream`) sobre la punta de `upstream`.
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
