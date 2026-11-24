//! Getting a peer's repository here: a new one made for its state, and the
//! objects its state needs.

use super::{GitState, config, valid_branch};
use crate::git::repo::{HEADS, HeadTarget, Oid, Repo, TAGS};
use anyhow::{Context, Result, bail};
use std::path::Path;

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

/// Copies the objects `wants` names from `from` into `to`, as a pack. For
/// tests and local transfers; peers stream the same pack over the network.
pub fn copy_objects(from: &Repo, to: &Repo, wants: &[Oid], haves: &[Oid]) -> Result<()> {
    if wants.is_empty() {
        return Ok(());
    }

    let mut pack = from.pack(wants, haves)?;
    let errors = crate::git::repo::drain_stderr(&mut pack);
    let mut unpack = to.unpack()?;
    let mut out = pack.stdout.take().context("pack-objects has no output")?;
    let mut input = unpack.stdin.take().context("index-pack has no input")?;
    std::io::copy(&mut out, &mut input)?;
    drop(input);
    let status = pack.wait()?;
    if !status.success() {
        bail!(
            "git pack-objects failed: {}",
            errors.join().unwrap_or_default().trim()
        );
    }
    crate::git::repo::finish(unpack, "index-pack")
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            Memory, advertise,
            tests::{pair, sync},
        },
        *,
    };
    use crate::git::repo::testing::git;

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
        assert_eq!(
            b.head_commit().expect("HEAD"),
            a.head_commit().expect("HEAD")
        );
        assert!(aligned.diverged.is_empty());
        assert!(aligned.checkout_moved);
    }

    #[test]
    fn a_repository_made_again_deletes_nothing_elsewhere() {
        let (_a_dir, a, b_dir, b) = pair();
        git(&a, &["branch", "topic"]);
        let (mut am, mut bm) = (Memory::default(), Memory::default());
        sync(&a, &mut am, &b, &mut bm);
        assert!(bm.known.contains_key("topic"));

        // b's .git is deleted and an empty repository takes its place.
        std::fs::remove_dir_all(b.root().join(".git")).expect("rm");
        let state = advertise(&a, &am).expect("state");
        let b = bootstrap(b_dir.path(), &state).expect("bootstrapped");
        let aligned = sync(&a, &mut am, &b, &mut bm);

        assert!(bm.tombstones.is_empty(), "{:?}", bm.tombstones);
        assert_eq!(b.refs(HEADS).expect("refs"), a.refs(HEADS).expect("refs"));
        assert!(aligned.diverged.is_empty());
    }
}
