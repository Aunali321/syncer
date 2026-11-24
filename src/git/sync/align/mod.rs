//! Settling a peer's branches, tags and HEAD into this repository.

mod branches;

use super::{Aligned, Diverged, GitState, Memory, valid_branch};
use crate::git::repo::{CONFLICTS, HEADS, HeadTarget, Repo, TAGS};
use anyhow::{Context, Result};
use branches::{take_branches, take_deletes};

/// Settles `peer`'s state into this repository, once the objects `wants`
/// named are here. `peer_label` names the peer in conflict refs.
pub fn align(
    repo: &Repo,
    memory: &mut Memory,
    peer: &GitState,
    peer_label: &str,
) -> Result<Aligned> {
    let mut aligned = Aligned::default();

    if let Some(operation) = repo.busy() {
        aligned.notes.push(format!(
            "waiting for a Git operation to finish ({operation})"
        ));
        return Ok(aligned);
    }

    let checked_out_moved = take_branches(repo, memory, peer, peer_label, &mut aligned)?;
    take_tags(repo, peer)?;
    follow_head(repo, memory, peer, &mut aligned)?;
    if checked_out_moved && !aligned.checkout_moved {
        repo.reset_index()?;
        aligned.checkout_moved = true;
    }
    // Deletes after HEAD has followed the peer, which may have moved off
    // the deleted branch.
    take_deletes(repo, memory, peer, &mut aligned)?;
    drop_stale_conflicts(repo, peer_label, &aligned)?;
    clear_resolved(repo)?;
    Ok(aligned)
}

/// The branch checked out here, if one is.
fn checked_out(repo: &Repo) -> Result<Option<String>> {
    Ok(match repo.head()? {
        HeadTarget::Branch(branch) => Some(branch),
        HeadTarget::Detached(_) => None,
    })
}

/// Creates the peer's tags that are not here. A tag is never moved.
fn take_tags(repo: &Repo, peer: &GitState) -> Result<()> {
    for (name, tip) in &peer.tags {
        let reference = format!("{TAGS}{name}");
        if !valid_branch(name) || repo.resolve(&reference)?.is_some() || !repo.complete(tip)? {
            continue;
        }
        repo.update_ref(&reference, tip, None)?;
    }

    Ok(())
}

/// Points HEAD where the peer's newer HEAD does, once that branch is here.
fn follow_head(
    repo: &Repo,
    memory: &mut Memory,
    peer: &GitState,
    aligned: &mut Aligned,
) -> Result<()> {
    if let Some(theirs) = &peer.head {
        memory.lamport = memory.lamport.max(theirs.stamp.lamport);
        let newer = memory
            .head
            .as_ref()
            .is_none_or(|ours| theirs.stamp > ours.stamp);

        if newer {
            if memory.head.as_ref().map(|ours| &ours.target) != Some(&theirs.target)
                || repo.head()? != theirs.target
            {
                match head_ready(repo, &theirs.target)? {
                    Ok(()) => {
                        repo.set_head(&theirs.target)?;
                        aligned.checkout_moved = true;
                        memory.head = Some(theirs.clone());
                    }
                    Err(why) => aligned.notes.push(why),
                }
            } else {
                memory.head = Some(theirs.clone());
            }
        }
    }

    Ok(())
}

/// A conflict ref for this peer stands only while the peer's branch still
/// diverges at that tip: once the peer moved on (merged, reset to ours,
/// deleted the branch), it goes.
fn drop_stale_conflicts(repo: &Repo, peer_label: &str, aligned: &Aligned) -> Result<()> {
    let prefix = format!("{CONFLICTS}{peer_label}/");
    for (reference, held) in repo.refs(&prefix)? {
        let Some(branch) = reference.strip_prefix(&prefix) else {
            continue;
        };
        let current = aligned
            .diverged
            .iter()
            .any(|diverged| diverged.branch == branch && diverged.peer_tip == held);
        if !current {
            repo.delete_ref(&reference, &held)?;
        }
    }

    Ok(())
}

/// Whether HEAD can point at `target` now, or why not.
fn head_ready(repo: &Repo, target: &HeadTarget) -> Result<Result<(), String>> {
    Ok(match target {
        HeadTarget::Branch(branch) if !valid_branch(branch) => {
            Err(format!("ignoring the branch name {branch:?}"))
        }
        HeadTarget::Branch(branch) if repo.resolve(&format!("{HEADS}{branch}"))?.is_some() => {
            Ok(())
        }
        // An unborn branch only in a repository with no commits yet, as a
        // new one is: anywhere else it would leave the checkout empty.
        HeadTarget::Branch(_) if repo.refs(HEADS)?.is_empty() => Ok(()),
        HeadTarget::Branch(branch) => Err(format!("waiting for the branch {branch} to arrive")),
        HeadTarget::Detached(oid) if repo.complete(oid)? => Ok(()),
        HeadTarget::Detached(oid) => Err(format!("the detached HEAD {oid:?} has not arrived")),
    })
}

/// The diverged branches, from the conflict refs still held.
pub fn diverged(repo: &Repo) -> Result<Vec<Diverged>> {
    let mut out = Vec::new();

    for (reference, tip) in repo.refs(CONFLICTS)? {
        let Some((_, branch)) = reference
            .strip_prefix(CONFLICTS)
            .and_then(|rest| rest.split_once('/'))
        else {
            continue;
        };
        out.push(Diverged {
            branch: branch.to_owned(),
            reference,
            peer_tip: tip,
        });
    }

    Ok(out)
}

/// Deletes the conflict refs whose tip the branch now contains (merged) or
/// whose branch is gone.
pub(super) fn clear_resolved(repo: &Repo) -> Result<()> {
    for conflict in diverged(repo)? {
        let resolved = match repo.resolve(&format!("{HEADS}{}", conflict.branch))? {
            Some(local) => repo.contains(&local, &conflict.peer_tip)?,
            None => true,
        };

        if resolved {
            repo.delete_ref(&conflict.reference, &conflict.peer_tip)
                .with_context(|| format!("clearing {}", conflict.reference))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            advertise, observe,
            tests::{A, B, pair, sync},
        },
        *,
    };
    use crate::git::repo::testing::{commit, git};

    #[test]
    fn head_follows_the_latest_switch_without_touching_files() {
        let (_a_dir, a, _b_dir, b) = pair();
        git(&a, &["branch", "topic"]);
        let (mut am, mut bm) = (Memory::default(), Memory::default());
        sync(&a, &mut am, &b, &mut bm);
        std::fs::write(b.root().join("file.txt"), "first").expect("write");

        git(&a, &["checkout", "-q", "topic"]);
        sync(&a, &mut am, &b, &mut bm);
        assert_eq!(b.head().expect("HEAD"), HeadTarget::Branch("topic".into()));
        assert_eq!(
            std::fs::read_to_string(b.root().join("file.txt")).expect("read"),
            "first"
        );

        // b switching back is newer than a's switch and wins on a.
        git(&b, &["checkout", "-q", "main"]);
        observe(&b, &mut bm, B).expect("observed");
        let b_state = advertise(&b, &bm).expect("state");
        observe(&a, &mut am, A).expect("observed");
        align(&a, &mut am, &b_state, "b").expect("aligned");
        assert_eq!(a.head().expect("HEAD"), HeadTarget::Branch("main".into()));
    }

    #[test]
    fn head_never_moves_to_a_branch_that_is_not_here() {
        let (_a_dir, a, _b_dir, b) = pair();
        let (mut am, mut bm) = (Memory::default(), Memory::default());
        sync(&a, &mut am, &b, &mut bm);

        // a checks out a branch with no commits yet.
        git(&a, &["checkout", "-q", "--orphan", "fresh"]);
        let aligned = sync(&a, &mut am, &b, &mut bm);
        assert_eq!(b.head().expect("HEAD"), HeadTarget::Branch("main".into()));
        assert!(aligned.notes.iter().any(|note| note.contains("fresh")));
    }

    #[test]
    fn nothing_moves_during_a_git_operation() {
        let (_a_dir, a, _b_dir, b) = pair();
        let (mut am, mut bm) = (Memory::default(), Memory::default());
        sync(&a, &mut am, &b, &mut bm);
        let before = b.head_commit().expect("HEAD");
        commit(&a, "ahead");

        std::fs::write(b.root().join(".git/MERGE_HEAD"), "x").expect("write");
        let aligned = sync(&a, &mut am, &b, &mut bm);
        assert_eq!(b.head_commit().expect("HEAD"), before);
        assert_eq!(aligned.notes.len(), 1);
    }
}
