//! Branches from a peer: created, fast-forwarded, kept apart when they
//! diverged, and deleted when the peer deleted them over work seen here.

use super::{
    super::{Aligned, Diverged, GitState, Memory, valid_branch},
    checked_out,
};
use crate::git::repo::{CONFLICTS, HEADS, HeadTarget, Repo};
use anyhow::Result;

/// Creates and fast-forwards branches to the peer's tips, keeping a
/// conflict ref for each that diverged. Whether the checked-out branch moved.
pub(super) fn take_branches(
    repo: &Repo,
    memory: &mut Memory,
    peer: &GitState,
    peer_label: &str,
    aligned: &mut Aligned,
) -> Result<bool> {
    let checked_out = checked_out(repo)?;
    let mut checked_out_moved = false;
    for (name, tip) in &peer.branches {
        if !valid_branch(name) {
            aligned
                .notes
                .push(format!("ignoring the branch name {name:?}"));
            continue;
        }
        if !repo.complete(tip)? {
            aligned
                .notes
                .push(format!("{name}: the peer's commits did not all arrive"));
            continue;
        }

        let local = repo.resolve(&format!("{HEADS}{name}"))?;
        let reference = format!("{HEADS}{name}");

        match local {
            None => {
                // A branch the peer has checked out, with a HEAD newer than
                // ours (so the checkout here follows it), is in use: that
                // beats a delete here, as an edit beats a delete for files.
                let checked_out_there = peer.head.as_ref().is_some_and(|theirs| {
                    theirs.target == HeadTarget::Branch(name.clone())
                        && memory
                            .head
                            .as_ref()
                            .is_none_or(|ours| theirs.stamp > ours.stamp)
                });
                if !checked_out_there
                    && let Some(deleted) = memory.tombstones.get(name)
                    && repo.has(deleted)?
                    && repo.contains(deleted, tip)?
                {
                    // Deleted here at or after the peer's tip: the peer deletes it.
                    continue;
                }
                repo.update_ref(&reference, tip, None)?;
                memory.tombstones.remove(name);
                memory.known.insert(name.clone(), tip.clone());
                // An unborn branch HEAD already named.
                checked_out_moved |= checked_out.as_deref() == Some(name.as_str());
            }
            Some(local) if local == *tip => {}
            Some(local) if repo.contains(tip, &local)? => {
                repo.update_ref(&reference, tip, Some(&local))?;
                memory.known.insert(name.clone(), tip.clone());
                checked_out_moved |= checked_out.as_deref() == Some(name.as_str());
            }
            Some(local) if repo.contains(&local, tip)? => {}
            Some(_) => {
                let conflict = format!("{CONFLICTS}{peer_label}/{name}");
                let held = repo.resolve(&conflict)?;
                if held.as_ref() != Some(tip) {
                    repo.update_ref(&conflict, tip, held.as_ref())?;
                }
                aligned.diverged.push(Diverged {
                    branch: name.clone(),
                    reference: conflict,
                    peer_tip: tip.clone(),
                });
            }
        }
    }

    Ok(checked_out_moved)
}

/// Deletes branches the peer deleted, unless there is work here the
/// deleting device never saw, or the branch is checked out here.
pub(super) fn take_deletes(
    repo: &Repo,
    memory: &mut Memory,
    peer: &GitState,
    aligned: &mut Aligned,
) -> Result<()> {
    let checked_out_now = checked_out(repo)?;
    for (name, deleted) in &peer.tombstones {
        if !valid_branch(name) || peer.branches.contains_key(name) || !repo.has(deleted)? {
            continue;
        }

        match repo.resolve(&format!("{HEADS}{name}"))? {
            Some(local) => {
                if !repo.contains(deleted, &local)? {
                    // Work the deleting device never saw: the branch stays.
                    continue;
                }
                if checked_out_now.as_deref() == Some(name.as_str()) {
                    aligned.notes.push(format!(
                        "{name} was deleted elsewhere but is checked out here"
                    ));
                    continue;
                }
                repo.delete_ref(&format!("{HEADS}{name}"), &local)?;
                memory.known.remove(name);
                memory.tombstones.insert(name.clone(), deleted.clone());
            }
            None => {
                let newer = match memory.tombstones.get(name) {
                    Some(ours) => {
                        repo.has(ours)? && repo.contains(deleted, ours)? && ours != deleted
                    }
                    None => true,
                };
                if newer {
                    memory.tombstones.insert(name.clone(), deleted.clone());
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            super::{
                advertise, copy_objects, observe,
                tests::{A, B, fill, pair, sync},
                wants,
            },
            align, diverged,
        },
        *,
    };
    use crate::git::repo::testing::{commit, git};

    #[test]
    fn branches_fast_forward_and_never_go_back() {
        let (_a_dir, a, _b_dir, b) = pair();
        let (mut am, mut bm) = (Memory::default(), Memory::default());
        sync(&a, &mut am, &b, &mut bm);

        let ahead = commit(&a, "ahead");
        sync(&a, &mut am, &b, &mut bm);
        assert_eq!(
            b.resolve("refs/heads/main").expect("ref"),
            Some(ahead.clone())
        );
        // The index follows: nothing staged against the new commit's tree but
        // the file itself, which file sync brings.
        assert_eq!(git(&b, &["diff", "--cached", "--name-only"]).trim(), "");

        // b is ahead now; a's older tip changes nothing on b.
        std::fs::write(b.root().join("file.txt"), "ahead").expect("write");
        let further = commit(&b, "further");
        let mut stale = am.clone();
        sync(&a, &mut stale, &b, &mut bm);
        assert_eq!(b.resolve("refs/heads/main").expect("ref"), Some(further));
    }

    #[test]
    fn diverged_branches_keep_both_tips_until_merged() {
        let (_a_dir, a, _b_dir, b) = pair();
        let (mut am, mut bm) = (Memory::default(), Memory::default());
        sync(&a, &mut am, &b, &mut bm);
        fill(&b);

        let theirs = commit(&a, "from a");
        std::fs::write(b.root().join("other.txt"), "b").expect("write");
        git(&b, &["add", "other.txt"]);
        git(&b, &["commit", "-q", "-m", "from b"]);
        let ours = b.head_commit().expect("HEAD").expect("a commit");

        let aligned = sync(&a, &mut am, &b, &mut bm);
        assert_eq!(aligned.diverged.len(), 1);
        assert_eq!(aligned.diverged[0].reference, "refs/syncer/a/main");
        assert_eq!(b.resolve("refs/heads/main").expect("ref"), Some(ours));
        assert_eq!(b.resolve("refs/syncer/a/main").expect("ref"), Some(theirs));

        git(
            &b,
            &[
                "merge",
                "-q",
                "--no-edit",
                "-X",
                "theirs",
                "refs/syncer/a/main",
            ],
        );
        observe(&b, &mut bm, B).expect("observed");
        assert!(diverged(&b).expect("diverged").is_empty());

        // a takes the merge as a fast-forward.
        let (b_state, merged) = (
            advertise(&b, &bm).expect("state"),
            b.head_commit().expect("HEAD"),
        );
        let (wants, haves) = wants(&a, &b_state).expect("wants");
        copy_objects(&b, &a, &wants, &haves).expect("copied");
        align(&a, &mut am, &b_state, "b").expect("aligned");
        assert_eq!(a.head_commit().expect("HEAD"), merged);
    }

    #[test]
    fn deletes_spread_only_over_work_the_deleter_saw() {
        let (_a_dir, a, _b_dir, b) = pair();
        git(&a, &["branch", "done"]);
        git(&a, &["branch", "busy"]);
        let (mut am, mut bm) = (Memory::default(), Memory::default());
        sync(&a, &mut am, &b, &mut bm);
        fill(&b);

        // b moves `busy` on before it hears of the delete.
        git(&b, &["checkout", "-q", "busy"]);
        std::fs::write(b.root().join("file.txt"), "busy work").expect("write");
        commit(&b, "busy work");
        git(&b, &["checkout", "-q", "main"]);
        observe(&b, &mut bm, B).expect("observed");

        git(&a, &["branch", "-D", "-q", "done", "busy"]);
        sync(&a, &mut am, &b, &mut bm);

        let branches = b.refs(HEADS).expect("refs");
        assert!(!branches.contains_key("refs/heads/done"));
        assert!(branches.contains_key("refs/heads/busy"));
        assert!(bm.tombstones.contains_key("done"));

        // And `busy` comes back to a, since it holds new work.
        let b_state = advertise(&b, &bm).expect("state");
        let (wants, haves) = wants(&a, &b_state).expect("wants");
        copy_objects(&b, &a, &wants, &haves).expect("copied");
        observe(&a, &mut am, A).expect("observed");
        align(&a, &mut am, &b_state, "b").expect("aligned");
        let branches = a.refs(HEADS).expect("refs");
        assert!(branches.contains_key("refs/heads/busy"));
        assert!(!branches.contains_key("refs/heads/done"));
    }

    #[test]
    fn a_branch_checked_out_elsewhere_beats_its_delete() {
        let (_a_dir, a, _b_dir, b) = pair();
        git(&a, &["branch", "topic"]);
        let (mut am, mut bm) = (Memory::default(), Memory::default());
        sync(&a, &mut am, &b, &mut bm);

        // b deletes topic while a checks it out.
        git(&b, &["branch", "-q", "-D", "topic"]);
        observe(&b, &mut bm, B).expect("observed");
        git(&a, &["checkout", "-q", "topic"]);
        sync(&a, &mut am, &b, &mut bm);

        assert!(
            b.refs(HEADS)
                .expect("refs")
                .contains_key("refs/heads/topic")
        );
        assert_eq!(b.head().expect("HEAD"), HeadTarget::Branch("topic".into()));
        assert!(
            b.head_commit().expect("HEAD").is_some(),
            "not an empty checkout"
        );
        assert!(!bm.tombstones.contains_key("topic"));
    }

    #[test]
    fn odd_names_from_peers_are_refused() {
        for good in ["main", "feature/x", "v1.2", "release-2024"] {
            assert!(valid_branch(good), "{good}");
        }
        for bad in [
            "", "-x", "a..b", "a b", "a~1", "x.lock", "a/", "/a", ".a", "a/.b", "a@{1}", "a\\b",
            "@",
        ] {
            assert!(!valid_branch(bad), "{bad}");
        }
    }
}
