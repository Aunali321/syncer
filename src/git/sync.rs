//! Keeping a checkout's branches, tags and HEAD in step with peers. See
//! DESIGN.md, "Git sync". Everything here blocks.

use super::{
    config,
    repo::{CONFLICTS, HEADS, HeadTarget, Oid, Repo, TAGS},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// Orders HEAD changes: the higher stamp wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Stamp {
    pub lamport: u64,
    pub device: [u8; 32],
}

/// What HEAD points at, and when that was decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadRecord {
    pub target: HeadTarget,
    pub stamp: Stamp,
}

/// What a device tells its peers about a folder's repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    pub head: Option<HeadRecord>,
    /// The commit HEAD is at, which file sync compares.
    pub head_commit: Option<Oid>,
    /// By short name.
    pub branches: BTreeMap<String, Oid>,
    pub tags: BTreeMap<String, Oid>,
    /// Deleted branches, by short name, with their tip when deleted.
    pub tombstones: BTreeMap<String, Oid>,
    /// The sanitized config a new device starts with.
    pub config: Vec<(String, String)>,
}

/// What a device remembers about its own repository between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    /// The branches as last seen, by short name.
    pub known: BTreeMap<String, Oid>,
    pub tombstones: BTreeMap<String, Oid>,
    pub head: Option<HeadRecord>,
    pub lamport: u64,
}

/// A diverged branch: the peer's tip, kept under a conflict ref.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diverged {
    pub branch: String,
    /// The conflict ref holding the peer's tip.
    pub reference: String,
    pub peer_tip: Oid,
}

/// What aligning with a peer did.
#[derive(Debug, Default)]
pub struct Aligned {
    /// HEAD or the checked-out branch moved, so the index was refreshed.
    pub checkout_moved: bool,
    pub diverged: Vec<Diverged>,
    /// Things left as they are, with why, for the status.
    pub notes: Vec<String>,
}

/// Reads the repository and records what the user changed since the last
/// look: deleted branches become tombstones and a HEAD that moved gets a new
/// stamp. Does nothing while a Git operation is under way. Returns whether
/// anything a peer would see changed.
pub fn observe(repo: &Repo, memory: &mut Memory, me: [u8; 32]) -> Result<bool> {
    if repo.busy().is_some() {
        return Ok(false);
    }

    let branches = short(repo.refs(HEADS)?, HEADS);
    let head = repo.head()?;
    let mut changed = branches != memory.known;

    for (name, tip) in &memory.known {
        if !branches.contains_key(name) {
            memory.tombstones.insert(name.clone(), tip.clone());
        }
    }

    memory.tombstones.retain(|name, _| !branches.contains_key(name));
    memory.known = branches;

    match &memory.head {
        Some(record) if record.target == head => {}
        Some(_) => {
            memory.lamport += 1;
            memory.head = Some(HeadRecord {
                target: head,
                stamp: Stamp {
                    lamport: memory.lamport,
                    device: me,
                },
            });
            changed = true;
        }
        None => {
            // The checkout as it was before syncing: any decision made since
            // beats it.
            memory.head = Some(HeadRecord {
                target: head,
                stamp: Stamp { lamport: 0, device: me },
            });
            changed = true;
        }
    }

    clear_resolved(repo)?;
    Ok(changed)
}

/// This device's state, to send to peers.
pub fn advertise(repo: &Repo, memory: &Memory) -> Result<GitState> {
    let config = repo
        .local_config()?
        .into_iter()
        .filter(|(key, value)| config::allowed(key, value))
        .collect();

    Ok(GitState {
        head: memory.head.clone(),
        head_commit: repo.head_commit()?,
        branches: memory.known.clone(),
        tags: short(repo.refs(TAGS)?, TAGS),
        tombstones: memory.tombstones.clone(),
        config,
    })
}

/// Makes a repository at `root` for a peer's state: its HEAD branch, its
/// sanitized config, and syncer's folder excluded.
pub fn bootstrap(root: &Path, peer: &GitState) -> Result<Repo> {
    let branch = match peer.head.as_ref().map(|head| &head.target) {
        Some(HeadTarget::Branch(branch)) if valid_branch(branch) => branch.clone(),
        _ => "main".to_owned(),
    };
    let repo = Repo::init(root, &branch)?;

    for (key, value) in &peer.config {
        if config::allowed(key, value) {
            repo.add_config(key, value)?;
        }
    }

    repo.exclude(&format!("/{}/", crate::folder::OWN_DIR))?;
    Ok(repo)
}

/// The objects a peer's state needs that are not here, and the tips that
/// tell the peer what to leave out of the pack.
pub fn wants(repo: &Repo, peer: &GitState) -> Result<(Vec<Oid>, Vec<Oid>)> {
    let mut wants = Vec::new();
    let detached = match peer.head.as_ref().map(|head| &head.target) {
        Some(HeadTarget::Detached(oid)) => Some(oid),
        _ => None,
    };

    for oid in peer
        .branches
        .values()
        .chain(peer.tags.values())
        .chain(peer.tombstones.values())
        .chain(detached)
    {
        if !wants.contains(oid) && !repo.has(oid)? {
            wants.push(oid.clone());
        }
    }

    let mut haves: Vec<Oid> = repo.refs(HEADS)?.into_values().collect();
    haves.extend(repo.refs(TAGS)?.into_values().take(64));
    haves.dedup();
    Ok((wants, haves))
}

/// Settles `peer`'s state into this repository, once the objects `wants`
/// named are here. `peer_label` names the peer in conflict refs.
pub fn align(repo: &Repo, memory: &mut Memory, peer: &GitState, peer_label: &str) -> Result<Aligned> {
    let mut aligned = Aligned::default();

    if let Some(operation) = repo.busy() {
        aligned.notes.push(format!("waiting for a Git operation to finish ({operation})"));
        return Ok(aligned);
    }

    let checked_out = match repo.head()? {
        HeadTarget::Branch(branch) => Some(branch),
        HeadTarget::Detached(_) => None,
    };
    let mut checked_out_moved = false;

    for (name, tip) in &peer.branches {
        if !valid_branch(name) {
            aligned.notes.push(format!("ignoring the branch name {name:?}"));
            continue;
        }
        if !repo.complete(tip)? {
            aligned.notes.push(format!("{name}: the peer's commits did not all arrive"));
            continue;
        }

        let local = repo.resolve(&format!("{HEADS}{name}"))?;
        let reference = format!("{HEADS}{name}");

        match local {
            None => {
                if let Some(deleted) = memory.tombstones.get(name)
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
                if checked_out.as_deref() == Some(name.as_str()) {
                    aligned.notes.push(format!("{name} was deleted elsewhere but is checked out here"));
                    continue;
                }
                repo.delete_ref(&format!("{HEADS}{name}"), &local)?;
                memory.known.remove(name);
                memory.tombstones.insert(name.clone(), deleted.clone());
            }
            None => {
                let newer = match memory.tombstones.get(name) {
                    Some(ours) => repo.has(ours)? && repo.contains(deleted, ours)? && ours != deleted,
                    None => true,
                };
                if newer {
                    memory.tombstones.insert(name.clone(), deleted.clone());
                }
            }
        }
    }

    for (name, tip) in &peer.tags {
        let reference = format!("{TAGS}{name}");
        if !valid_branch(name) || repo.resolve(&reference)?.is_some() || !repo.complete(tip)? {
            continue;
        }
        repo.update_ref(&reference, tip, None)?;
    }

    if let Some(theirs) = &peer.head {
        memory.lamport = memory.lamport.max(theirs.stamp.lamport);
        let newer = memory.head.as_ref().is_none_or(|ours| theirs.stamp > ours.stamp);

        if newer {
            if memory.head.as_ref().map(|ours| &ours.target) != Some(&theirs.target) || repo.head()? != theirs.target {
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

    if checked_out_moved && !aligned.checkout_moved {
        repo.reset_index()?;
        aligned.checkout_moved = true;
    }

    clear_resolved(repo)?;
    Ok(aligned)
}

/// Whether HEAD can point at `target` now, or why not.
fn head_ready(repo: &Repo, target: &HeadTarget) -> Result<Result<(), String>> {
    Ok(match target {
        HeadTarget::Branch(branch) if !valid_branch(branch) => Err(format!("ignoring the branch name {branch:?}")),
        // An unborn branch is fine: a new repository's HEAD names one.
        HeadTarget::Branch(_) => Ok(()),
        HeadTarget::Detached(oid) if repo.complete(oid)? => Ok(()),
        HeadTarget::Detached(oid) => Err(format!("the detached HEAD {oid:?} has not arrived")),
    })
}

/// The diverged branches, from the conflict refs still held.
pub fn diverged(repo: &Repo) -> Result<Vec<Diverged>> {
    let mut out = Vec::new();

    for (reference, tip) in repo.refs(CONFLICTS)? {
        let Some((_, branch)) = reference.strip_prefix(CONFLICTS).and_then(|rest| rest.split_once('/')) else {
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
fn clear_resolved(repo: &Repo) -> Result<()> {
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

fn short(refs: BTreeMap<String, Oid>, prefix: &str) -> BTreeMap<String, Oid> {
    refs.into_iter()
        .filter_map(|(name, oid)| Some((name.strip_prefix(prefix)?.to_owned(), oid)))
        .collect()
}

/// A conservative check of a branch or tag name from a peer, stricter than
/// `git check-ref-format`.
pub fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && !name.contains("/.")
        && name != "@"
        && name
            .chars()
            .all(|c| !c.is_control() && !matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

/// A peer label for conflict refs: the first characters of its id.
pub fn label(device: &[u8; 32]) -> String {
    hex::encode(&device[..5])
}

/// Copies the objects `wants` names from `from` into `to`, as a pack. For
/// tests and local transfers; peers stream the same pack over the network.
pub fn copy_objects(from: &Repo, to: &Repo, wants: &[Oid], haves: &[Oid]) -> Result<()> {
    if wants.is_empty() {
        return Ok(());
    }

    let mut pack = from.pack(wants, haves)?;
    let errors = super::repo::drain_stderr(&mut pack);
    let mut unpack = to.unpack()?;
    let mut out = pack.stdout.take().context("pack-objects has no output")?;
    let mut input = unpack.stdin.take().context("index-pack has no input")?;
    std::io::copy(&mut out, &mut input)?;
    drop(input);
    let status = pack.wait()?;
    if !status.success() {
        bail!("git pack-objects failed: {}", errors.join().unwrap_or_default().trim());
    }
    super::repo::finish(unpack, "index-pack")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::repo::testing::{commit, git, repo};

    const A: [u8; 32] = [1; 32];
    const B: [u8; 32] = [2; 32];

    /// Brings `peer`'s state into `local`, objects first.
    fn sync(from: &Repo, from_memory: &mut Memory, to: &Repo, to_memory: &mut Memory) -> Aligned {
        observe(from, from_memory, A).expect("observed");
        observe(to, to_memory, B).expect("observed");
        let state = advertise(from, from_memory).expect("advertised");
        let (wants, haves) = wants(to, &state).expect("wants");
        copy_objects(from, to, &wants, &haves).expect("copied");
        align(to, to_memory, &state, "a").expect("aligned")
    }

    /// Writes the checked-out files, as file sync would.
    fn fill(repo: &Repo) {
        git(repo, &["checkout", "-q", "--", "."]);
    }

    /// Two repositories with the same first commit.
    fn pair() -> (tempfile::TempDir, Repo, tempfile::TempDir, Repo) {
        let (a_dir, a) = repo();
        let b_dir = tempfile::tempdir().expect("a temp dir");
        let state = {
            let mut memory = Memory::default();
            observe(&a, &mut memory, A).expect("observed");
            advertise(&a, &memory).expect("advertised")
        };
        let b = bootstrap(b_dir.path(), &state).expect("bootstrapped");
        git(&b, &["config", "user.name", "Test"]);
        git(&b, &["config", "user.email", "test@example.com"]);
        (a_dir, a, b_dir, b)
    }

    #[test]
    fn a_new_device_gets_every_branch_and_head() {
        let (_a_dir, a, _b_dir, b) = pair();
        git(&a, &["branch", "topic"]);
        git(&a, &["tag", "v1"]);
        let (mut am, mut bm) = (Memory::default(), Memory::default());

        let aligned = sync(&a, &mut am, &b, &mut bm);

        assert_eq!(b.refs(HEADS).expect("refs"), a.refs(HEADS).expect("refs"));
        assert_eq!(b.refs(TAGS).expect("refs"), a.refs(TAGS).expect("refs"));
        assert_eq!(b.head().expect("HEAD"), HeadTarget::Branch("main".into()));
        assert_eq!(b.head_commit().expect("HEAD"), a.head_commit().expect("HEAD"));
        assert!(aligned.diverged.is_empty());
        assert!(aligned.checkout_moved);
    }

    #[test]
    fn branches_fast_forward_and_never_go_back() {
        let (_a_dir, a, _b_dir, b) = pair();
        let (mut am, mut bm) = (Memory::default(), Memory::default());
        sync(&a, &mut am, &b, &mut bm);

        let ahead = commit(&a, "ahead");
        sync(&a, &mut am, &b, &mut bm);
        assert_eq!(b.resolve("refs/heads/main").expect("ref"), Some(ahead.clone()));
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
        assert_eq!(aligned.diverged[0].reference, "refs/syncer/conflicts/a/main");
        assert_eq!(b.resolve("refs/heads/main").expect("ref"), Some(ours));
        assert_eq!(b.resolve("refs/syncer/conflicts/a/main").expect("ref"), Some(theirs));

        git(&b, &["merge", "-q", "--no-edit", "-X", "theirs", "refs/syncer/conflicts/a/main"]);
        observe(&b, &mut bm, B).expect("observed");
        assert!(diverged(&b).expect("diverged").is_empty());

        // a takes the merge as a fast-forward.
        let (b_state, merged) = (advertise(&b, &bm).expect("state"), b.head_commit().expect("HEAD"));
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
    fn head_follows_the_latest_switch_without_touching_files() {
        let (_a_dir, a, _b_dir, b) = pair();
        git(&a, &["branch", "topic"]);
        let (mut am, mut bm) = (Memory::default(), Memory::default());
        sync(&a, &mut am, &b, &mut bm);
        std::fs::write(b.root().join("file.txt"), "first").expect("write");

        git(&a, &["checkout", "-q", "topic"]);
        sync(&a, &mut am, &b, &mut bm);
        assert_eq!(b.head().expect("HEAD"), HeadTarget::Branch("topic".into()));
        assert_eq!(std::fs::read_to_string(b.root().join("file.txt")).expect("read"), "first");

        // b switching back is newer than a's switch and wins on a.
        git(&b, &["checkout", "-q", "main"]);
        observe(&b, &mut bm, B).expect("observed");
        let b_state = advertise(&b, &bm).expect("state");
        observe(&a, &mut am, A).expect("observed");
        align(&a, &mut am, &b_state, "b").expect("aligned");
        assert_eq!(a.head().expect("HEAD"), HeadTarget::Branch("main".into()));
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

    #[test]
    fn odd_names_from_peers_are_refused() {
        for good in ["main", "feature/x", "v1.2", "release-2024"] {
            assert!(valid_branch(good), "{good}");
        }
        for bad in ["", "-x", "a..b", "a b", "a~1", "x.lock", "a/", "/a", ".a", "a/.b", "a@{1}", "a\\b", "@"] {
            assert!(!valid_branch(bad), "{bad}");
        }
    }
}
