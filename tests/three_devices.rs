//! Three devices on this machine, each a real `syncer run` process with its
//! own home, talking over loopback without relays: pairing, a shared Git
//! folder, and the cases that break byte-level sync tools.

mod common;

use common::{Device, all, wait};
use std::{fs, time::Duration};

#[test]
fn three_devices_share_a_git_project() {
    // SYNCER_TEST_KEEP=1 keeps the devices' directories and logs.
    let root = tempfile::Builder::new()
        .prefix("syncer-test")
        .disable_cleanup(std::env::var_os("SYNCER_TEST_KEEP").is_some())
        .tempdir()
        .expect("a temp dir");
    eprintln!("devices in {}", root.path().display());
    let mut a = Device::new(root.path(), "a");
    let mut b = Device::new(root.path(), "b");
    let mut c = Device::new(root.path(), "c");
    for device in [&mut a, &mut b, &mut c] {
        device.start();
    }

    // Pairing: b and c join through a.
    let ticket = a.syncer(&["invite"]);
    b.syncer(&["join", ticket.trim()]);
    let ticket = a.syncer(&["invite"]);
    c.syncer(&["join", ticket.trim()]);
    assert!(
        b.try_syncer(&["join", ticket.trim()]).is_err(),
        "an invite works once"
    );

    // A project with history, branches, a tag and ignored build output.
    fs::create_dir_all(&a.folder).expect("mkdir");
    a.git(&["init", "-q", "-b", "main"]);
    a.write("readme.md", "hello\n");
    a.write("src/main.rs", "fn main() {}\n");
    a.write(".gitignore", "target/\n");
    a.write("target/build.out", "big binary\n");
    a.write("target/pinned.txt", "tracked though ignored\n");
    a.git(&["add", "-A"]);
    a.git(&["add", "-f", "target/pinned.txt"]);
    a.git(&["commit", "-q", "-m", "init"]);
    a.git(&["branch", "feature"]);
    a.git(&["tag", "v1"]);
    a.syncer(&[
        "add",
        a.folder.to_str().expect("utf-8"),
        "--label",
        "project",
    ]);

    wait("b to see the offer", || {
        b.syncer(&["status"]).contains("run: syncer accept project")
    });
    b.syncer(&["accept", "project", b.folder.to_str().expect("utf-8")]);
    c.syncer(&["accept", "project", c.folder.to_str().expect("utf-8")]);

    all(&[&b, &c], "the project to arrive", |d| {
        d.read("src/main.rs").is_some() && d.folder.join(".git").is_dir() && d.clean()
    });
    for device in [&b, &c] {
        assert_eq!(
            device.git(&["branch", "--format=%(refname:short)"]),
            "feature\nmain\n"
        );
        assert_eq!(device.git(&["tag"]), "v1\n");
        assert_eq!(device.head(), a.head());
        assert!(
            device.read("target/build.out").is_none(),
            "ignored files stay home"
        );
        assert_eq!(
            device.read("target/pinned.txt").as_deref(),
            Some("tracked though ignored\n"),
            "tracked files sync whatever .gitignore says"
        );
    }

    // An edit anywhere reaches everyone; a commit moves every checkout.
    b.write("readme.md", "hello\nfrom b\n");
    all(&[&a, &c], "b's edit", |d| {
        d.read("readme.md").as_deref() == Some("hello\nfrom b\n")
    });
    c.git(&["commit", "-q", "-am", "edit from b"]);
    let committed = c.head();
    all(&[&a, &b], "c's commit", |d| {
        d.head() == committed && d.clean()
    });

    // A new branch, checked out on a, is checked out everywhere.
    a.git(&["checkout", "-q", "-b", "topic"]);
    a.write("topic.txt", "topic work\n");
    a.git(&["add", "topic.txt"]);
    a.git(&["commit", "-q", "-m", "topic work"]);
    all(&[&b, &c], "the topic branch", |d| {
        d.git(&["symbolic-ref", "--short", "HEAD"]).trim() == "topic"
            && d.read("topic.txt").is_some()
            && d.clean()
    });

    // Switching back and deleting a branch spread too.
    b.git(&["checkout", "-q", "main"]);
    c.git(&["branch", "-q", "-D", "feature"]);
    all(&[&a, &b, &c], "main and no feature branch", |d| {
        d.git(&["symbolic-ref", "--short", "HEAD"]).trim() == "main"
            && d.git(&["branch", "--format=%(refname:short)"]) == "main\ntopic\n"
            && d.read("topic.txt").is_none()
            && d.clean()
    });

    // Concurrent edits while b is away: one keeps the path, the other
    // becomes the same conflict copy everywhere.
    b.stop();
    a.write("notes.txt", "a's notes\n");
    std::thread::sleep(Duration::from_millis(50));
    b.write("notes.txt", "b's notes\n");
    b.start();
    all(&[&a, &b, &c], "one conflict copy", |d| {
        d.read("notes.txt").as_deref() == Some("b's notes\n") && d.conflict_copies().len() == 1
    });
    let copy = a.conflict_copies().remove(0);
    assert_eq!(b.conflict_copies(), vec![copy.clone()]);
    assert_eq!(a.read(&copy).as_deref(), Some("a's notes\n"));
    fs::remove_file(c.folder.join(&copy)).expect("the copy is removed");
    all(&[&a, &b], "the copy's removal", |d| {
        d.conflict_copies().is_empty()
    });
    a.git(&["add", "notes.txt"]);
    a.git(&["commit", "-q", "-m", "notes"]);
    let notes = a.head();
    all(&[&b, &c], "the notes commit", |d| {
        d.head() == notes && d.clean()
    });

    // Commits on both sides while c is away: the branches diverge, both tips
    // are kept, and the two work trees do not mix.
    c.stop();
    a.write("from-a.txt", "a\n");
    a.git(&["add", "from-a.txt"]);
    a.git(&["commit", "-q", "-m", "a's work"]);
    c.write("from-c.txt", "c\n");
    c.git(&["add", "from-c.txt"]);
    c.git(&["commit", "-q", "-m", "c's work"]);
    let c_tip = c.head();
    c.start();
    all(&[&a, &c], "the divergence to be noticed", |d| {
        !d.conflict_refs().is_empty()
    });
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        c.read("from-a.txt").is_none(),
        "a's files wait while histories differ"
    );
    assert!(
        a.read("from-c.txt").is_none(),
        "c's files wait while histories differ"
    );
    assert_eq!(c.head(), c_tip);
    assert!(c.syncer(&["status"]).contains("run: git merge syncer/"));

    // Merging on a settles it for everyone, without restarting anything.
    let theirs = a
        .conflict_refs()
        .lines()
        .next()
        .expect("a conflict ref")
        .to_owned();
    a.git(&["merge", "-q", "--no-edit", &theirs]);
    let merged = a.head();
    all(&[&a, &b, &c], "the merge everywhere", |d| {
        d.head() == merged
            && d.read("from-a.txt").is_some()
            && d.read("from-c.txt").is_some()
            && d.conflict_refs().is_empty()
            && d.clean()
    });
    for device in [&a, &b, &c] {
        assert!(
            device.conflict_copies().is_empty(),
            "{}: {:?}",
            device.name,
            device.conflict_copies()
        );
    }

    // An empty directory where b's folder was (a disk not mounted, say) is
    // refused; nothing is deleted anywhere.
    b.stop();
    let moved = b.folder.with_extension("away");
    fs::rename(&b.folder, &moved).expect("moved away");
    fs::create_dir(&b.folder).expect("an empty directory");
    b.start();
    wait("b to refuse the folder", || {
        b.syncer(&["status"])
            .contains("folder not found. If the drive is connected, run: syncer reset project")
    });
    std::thread::sleep(Duration::from_secs(3));
    assert!(a.read("readme.md").is_some() && c.read("readme.md").is_some());

    // With the disk back, b carries on.
    b.stop();
    fs::remove_dir(&b.folder).expect("the empty directory goes");
    fs::rename(&moved, &b.folder).expect("moved back");
    b.start();
    a.write("after.txt", "after the remount\n");
    all(&[&b, &c], "a file after the remount", |d| {
        d.read("after.txt").is_some()
    });

    for device in [&mut a, &mut b, &mut c] {
        device.stop();
    }
}
