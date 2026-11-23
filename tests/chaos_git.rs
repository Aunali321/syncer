//! Git doing everything people do to it: rewriting history, stopping
//! halfway through a rebase, detaching HEAD, deleting the branch another
//! machine is on, crashing with a lock held, garbage collecting, starting or
//! deleting a repository inside a synced folder.

mod common;

use common::{Device, all, cluster, converge, share, wait, wait_for};
use std::{fs, time::Duration};

const MINUTE: Duration = Duration::from_secs(60);

/// Makes a repository with a few commits on the first device, shares it,
/// and waits for every device to have it.
fn shared_repo(devices: &[Device]) {
    let a = &devices[0];
    fs::create_dir_all(&a.folder).expect("mkdir");
    a.git(&["init", "-q", "-b", "main"]);
    for i in 0..3 {
        a.write("file.txt", &format!("version {i}\n"));
        a.write(&format!("src/mod{i}.rs"), "// code\n");
        a.git(&["add", "-A"]);
        a.git(&["commit", "-q", "-m", &format!("commit {i}")]);
    }
    share(devices, &[]);
    let head = a.head();
    all(&devices.iter().collect::<Vec<_>>(), "the repository", |d| {
        d.folder.join(".git").is_dir()
            && d.try_git(&["rev-parse", "HEAD"])
                .is_ok_and(|h| h.trim() == head)
            && d.clean()
    });
}

/// Waits until every device has the same branches, tags, HEAD and files,
/// with no conflict refs and a clean status.
fn settled(devices: &[&Device]) {
    all(devices, "the same Git state everywhere", |d| {
        d.git_view() == devices[0].git_view() && d.conflict_refs().is_empty() && d.clean()
    });
    converge(devices, MINUTE);
}

#[test]
fn a_rewritten_history_is_kept_as_a_conflict_until_accepted() {
    let (_root, mut devices) = cluster(2);
    shared_repo(&devices);

    // a amends the last commit while b is away and commits on the old one.
    devices[1].stop();
    let a = &devices[0];
    a.write("file.txt", "amended\n");
    a.git(&["commit", "-q", "-a", "--amend", "-m", "amended"]);
    let rewritten = a.head();
    devices[1].start();
    let (a, b) = (&devices[0], &devices[1]);

    all(&[a, b], "the divergence", |d| !d.conflict_refs().is_empty());
    assert_ne!(b.head(), rewritten, "b's history is never rewritten for it");

    // b accepts the rewrite.
    let theirs = b
        .conflict_refs()
        .lines()
        .next()
        .expect("a conflict ref")
        .to_owned();
    b.git(&["reset", "-q", "--hard", &theirs]);
    settled(&[a, b]);
    assert_eq!(b.head(), rewritten);
}

#[test]
fn a_rebase_stopped_on_a_conflict_is_left_alone() {
    let (_root, devices) = cluster(2);
    shared_repo(&devices);
    let (a, b) = (&devices[0], &devices[1]);

    // b starts a rebase that stops on a conflict.
    b.git(&["checkout", "-q", "-b", "side", "HEAD~1"]);
    b.write("file.txt", "side change\n");
    b.git(&["commit", "-q", "-am", "side"]);
    let _ = b.try_git(&["rebase", "main"]);
    assert!(
        b.folder.join(".git/rebase-merge").exists() || b.folder.join(".git/rebase-apply").exists()
    );
    let b_head = b.git(&["rev-parse", "HEAD"]);

    // Meanwhile a commits.
    a.write("other.txt", "a keeps working\n");
    a.git(&["add", "other.txt"]);
    a.git(&["commit", "-q", "-m", "a's work"]);
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(
        b.git(&["rev-parse", "HEAD"]),
        b_head,
        "syncer did not move b mid-rebase"
    );
    assert!(
        a.try_git(&["symbolic-ref", "HEAD"]).is_ok(),
        "a did not follow b's detached mid-rebase HEAD"
    );
    assert!(b.status().contains("Git operation"), "{}", b.status());

    b.git(&["rebase", "--abort"]);
    b.git(&["checkout", "-q", "main"]);
    settled(&[a, b]);
}

#[test]
fn a_detached_head_is_followed() {
    let (_root, devices) = cluster(2);
    shared_repo(&devices);
    let (a, b) = (&devices[0], &devices[1]);

    a.git(&["checkout", "-q", "--detach", "HEAD~2"]);
    all(&[b], "b to detach too", |d| {
        d.try_git(&["symbolic-ref", "-q", "HEAD"]).is_err()
    });
    settled(&[a, b]);
    assert_eq!(b.read("file.txt").as_deref(), Some("version 0\n"));

    b.git(&["checkout", "-q", "main"]);
    settled(&[a, b]);
    assert_eq!(a.read("file.txt").as_deref(), Some("version 2\n"));
}

#[test]
fn a_branch_deleted_while_another_device_is_on_it() {
    let (_root, mut devices) = cluster(2);
    shared_repo(&devices);
    devices[0].git(&["branch", "feature"]);
    devices[0].git(&["checkout", "-q", "feature"]);
    all(&[&devices[1]], "b on feature", |d| {
        d.git(&["symbolic-ref", "--short", "HEAD"]).trim() == "feature"
    });

    // b is away; a leaves feature and deletes it.
    devices[1].stop();
    devices[0].git(&["checkout", "-q", "main"]);
    devices[0].git(&["branch", "-q", "-D", "feature"]);
    devices[1].start();
    let (a, b) = (&devices[0], &devices[1]);

    settled(&[a, b]);
    assert!(!b.git(&["branch"]).contains("feature"));
}

#[test]
fn work_on_a_deleted_branch_brings_it_back() {
    let (_root, mut devices) = cluster(2);
    shared_repo(&devices);
    devices[0].git(&["branch", "keep-me"]);
    all(&[&devices[1]], "the branch", |d| {
        d.git(&["branch"]).contains("keep-me")
    });

    devices[1].stop();
    devices[0].git(&["branch", "-q", "-D", "keep-me"]);
    // b commits on it without knowing.
    let b = &devices[1];
    b.git(&["checkout", "-q", "keep-me"]);
    b.write("important.txt", "do not lose\n");
    b.git(&["add", "important.txt"]);
    b.git(&["commit", "-q", "-m", "important"]);
    b.git(&["checkout", "-q", "main"]);
    devices[1].start();
    let (a, b) = (&devices[0], &devices[1]);

    all(&[a], "the branch to come back", |d| {
        d.git(&["branch"]).contains("keep-me")
    });
    settled(&[a, b]);
    assert!(
        a.git(&["log", "--oneline", "keep-me"])
            .contains("important")
    );
}

#[test]
fn the_same_branch_made_twice_while_apart_is_a_conflict() {
    let (_root, mut devices) = cluster(2);
    shared_repo(&devices);
    for device in &mut devices {
        device.stop();
    }
    for (device, text) in devices.iter().zip(["from a\n", "from b\n"]) {
        device.git(&["checkout", "-q", "-b", "topic"]);
        device.write("topic.txt", text);
        device.git(&["add", "topic.txt"]);
        device.git(&["commit", "-q", "-m", text.trim()]);
        device.git(&["checkout", "-q", "main"]);
    }
    for device in &mut devices {
        device.start();
    }
    let (a, b) = (&devices[0], &devices[1]);

    all(&[a, b], "topic to diverge", |d| {
        d.conflict_refs().contains("/topic")
    });
    // main is fine: files still flow.
    a.write("still-syncing.txt", "yes\n");
    wait("files to flow while topic diverges", || {
        b.read("still-syncing.txt").is_some()
    });
    fs::remove_file(a.path("still-syncing.txt")).expect("rm");

    let theirs = a.conflict_refs().lines().next().expect("a ref").to_owned();
    a.git(&["checkout", "-q", "topic"]);
    let _ = a.try_git(&["merge", "-q", "--no-edit", "-X", "ours", &theirs]);
    a.git(&["checkout", "-q", "main"]);
    settled(&[a, b]);
}

#[test]
fn a_stale_index_lock_holds_git_back_until_it_is_gone() {
    let (_root, devices) = cluster(2);
    shared_repo(&devices);
    let (a, b) = (&devices[0], &devices[1]);

    // b's git crashed and left its lock.
    fs::write(b.folder.join(".git/index.lock"), "").expect("lock");
    a.write("file.txt", "while b is locked\n");
    a.git(&["commit", "-q", "-am", "while locked"]);
    let tip = a.head();
    wait("b to report the lock", || b.status().contains("index.lock"));
    assert_ne!(b.head(), tip);

    fs::remove_file(b.folder.join(".git/index.lock")).expect("unlock");
    wait_for(
        "b to catch up after the lock went",
        Duration::from_secs(90),
        || b.head() == tip && b.clean(),
    );
    settled(&[a, b]);
}

#[test]
fn gc_packed_refs_tags_and_many_branches() {
    let (_root, devices) = cluster(2);
    shared_repo(&devices);
    let (a, b) = (&devices[0], &devices[1]);

    for i in 0..150 {
        a.git(&["branch", &format!("many/branch-{i:03}")]);
    }
    a.git(&["tag", "-a", "v1.0", "-m", "an annotated tag"]);
    a.git(&["tag", "lightweight"]);
    a.git(&["pack-refs", "--all"]);
    a.git(&["gc", "-q", "--prune=now"]);
    for i in 0..40 {
        a.write("file.txt", &format!("history {i}\n"));
        a.git(&["commit", "-q", "-am", &format!("history {i}")]);
    }
    settled(&[a, b]);
    assert_eq!(b.git(&["branch", "--list", "many/*"]).lines().count(), 150);
    assert_eq!(b.git(&["cat-file", "-t", "v1.0"]).trim(), "tag");
    b.git(&["fsck", "--no-progress", "--connectivity-only"]);

    // A moved tag stays where it was elsewhere: tags never move.
    let before = b.git(&["rev-parse", "lightweight"]);
    a.git(&["tag", "-f", "lightweight", "HEAD~5"]);
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(b.git(&["rev-parse", "lightweight"]), before);
}

#[test]
fn uncommitted_work_survives_commits_from_elsewhere() {
    let (_root, devices) = cluster(2);
    shared_repo(&devices);
    let (a, b) = (&devices[0], &devices[1]);

    b.write("src/mod0.rs", "// b is editing this\n");
    wait("b's edit to arrive", || {
        a.read("src/mod0.rs").as_deref() == Some("// b is editing this\n")
    });
    a.write("src/mod1.rs", "// a's change\n");
    a.git(&["add", "src/mod1.rs"]);
    a.git(&["commit", "-q", "-m", "a's change"]);
    let tip = a.head();

    all(&[b], "the commit", |d| d.head() == tip);
    converge(&[a, b], MINUTE);
    assert_eq!(
        b.read("src/mod0.rs").as_deref(),
        Some("// b is editing this\n")
    );
    assert_eq!(b.git(&["status", "--porcelain"]).trim(), "M src/mod0.rs");
}

#[test]
fn git_init_in_a_synced_folder_spreads_and_a_deleted_git_comes_back() {
    let (_root, devices) = cluster(2);
    share(&devices, &[]);
    let (a, b) = (&devices[0], &devices[1]);
    a.write("readme.md", "plain folder\n");
    converge(&[a, b], MINUTE);

    a.git(&["init", "-q", "-b", "main"]);
    a.git(&["add", "-A"]);
    a.git(&["commit", "-q", "-m", "now a repository"]);
    all(&[b], "b's repository", |d| {
        d.try_git(&["rev-parse", "HEAD"]).is_ok() && d.clean()
    });

    a.git(&["branch", "side"]);
    all(&[b], "the side branch", |d| {
        d.git(&["branch"]).contains("side")
    });

    // Someone deletes .git on b: it is not mirrored as a delete, b gets the
    // repository back from a, and no branch is deleted anywhere.
    fs::remove_dir_all(b.folder.join(".git")).expect("rm .git");
    wait("a to keep its repository", || {
        a.folder.join(".git/HEAD").exists()
    });
    b.syncer(&["rescan", "shared"]);
    all(&[b], "b's repository back", |d| {
        d.folder.join(".git").is_dir()
            && d.try_git(&["rev-parse", "HEAD"])
                .is_ok_and(|h| h.trim() == a.head())
    });
    all(&[b], "every branch back", |d| {
        d.git(&["branch"]).contains("side")
    });
    std::thread::sleep(Duration::from_secs(3));
    assert!(a.git(&["branch"]).contains("side"), "a kept its branches");
}

#[test]
fn nested_repositories_stay_home() {
    let (_root, devices) = cluster(2);
    shared_repo(&devices);
    let (a, b) = (&devices[0], &devices[1]);

    let nested = a.folder.join("vendor/lib");
    fs::create_dir_all(&nested).expect("mkdir");
    common::git(&nested, &["init", "-q"]);
    fs::write(nested.join("lib.c"), "int x;\n").expect("write");
    a.write("after-nested.txt", "marker\n");
    wait("a later file", || b.read("after-nested.txt").is_some());
    assert!(!b.folder.join("vendor/lib").exists());
}

#[test]
fn a_storm_of_commits_branches_and_switches_settles() {
    let (_root, devices) = cluster(3);
    shared_repo(&devices);
    let mut rng = common::Rng::new(common::seed(7));
    let started = std::time::Instant::now();
    let mut op = 0;

    while started.elapsed() < Duration::from_secs(20) {
        let device = &devices[rng.below(devices.len())];
        op += 1;
        match rng.below(6) {
            0 | 1 => {
                device.write(
                    &format!("work/{}.txt", rng.below(5)),
                    &format!("{} edit {op}\n", device.name),
                );
            }
            2 => {
                let _ = device.try_git(&["add", "-A"]);
                let _ = device.try_git(&[
                    "commit",
                    "-q",
                    "-m",
                    &format!("{} commit {op}", device.name),
                ]);
            }
            3 => {
                let _ = device.try_git(&["branch", &format!("b{}", rng.below(4))]);
            }
            4 => {
                // Switching with uncommitted work is refused by Git; fine.
                let target = *rng.pick(&["main", "b0", "b1", "b2", "b3"]);
                let _ = device.try_git(&["checkout", "-q", target]);
            }
            _ => {
                let _ = device.try_git(&["branch", "-q", "-D", &format!("b{}", rng.below(4))]);
            }
        }
        std::thread::sleep(Duration::from_millis(rng.below(150) as u64));
    }
    eprintln!("{op} operations");

    // Settle what diverged the way a person would: on one machine, merge
    // what it shows as diverged, until nothing is.
    let a = &devices[0];
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    loop {
        std::thread::sleep(Duration::from_secs(5));
        for reference in a
            .conflict_refs()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
        {
            let branch = reference.rsplit('/').next().unwrap_or_default().to_owned();
            let back = a
                .try_git(&["symbolic-ref", "--short", "HEAD"])
                .unwrap_or_default();
            // Work in progress is committed, not stashed: a stash that does
            // not apply back leaves the index unmerged and wedges the test.
            let _ = a.try_git(&["add", "-A"]);
            let _ = a.try_git(&["commit", "-q", "-m", "save work"]);
            if a.try_git(&["checkout", "-q", &branch]).is_ok() {
                if a.try_git(&["merge", "-q", "--no-edit", "-X", "ours", &reference])
                    .is_err()
                {
                    let _ = a.try_git(&["merge", "--abort"]);
                    let _ = a.try_git(&["merge", "-q", "--no-edit", "-s", "ours", &reference]);
                }
                let _ = a.try_git(&["checkout", "-q", back.trim()]);
            }
        }
        let quiet = devices
            .iter()
            .all(|d| d.conflict_refs().is_empty() && d.git_view() == a.git_view());
        if quiet {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the storm never settled:\n{}",
            devices
                .iter()
                .map(|d| format!(
                    "{}:\n{}{}\n{}",
                    d.name,
                    d.git_view(),
                    d.conflict_refs(),
                    d.status()
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    converge(
        &devices.iter().collect::<Vec<_>>(),
        Duration::from_secs(120),
    );
    common::all(
        &devices.iter().collect::<Vec<_>>(),
        "nothing left to do",
        |d| d.status().contains("up to date"),
    );
}
