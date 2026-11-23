//! Hostile file activity: random edits on devices that cannot see each
//! other, storms of concurrent writes, crashes, odd names and file types.
//! Every test ends with the same two checks: every device holds exactly the
//! same files, and nothing a device wrote was lost.

mod common;

use common::{Node, Rng, cluster, converge, share, tree, wait};
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    time::{Duration, Instant},
};

/// Every regular file's content in a tree.
fn contents(tree: &std::collections::BTreeMap<String, Node>) -> BTreeSet<Vec<u8>> {
    tree.values()
        .filter_map(|node| match node {
            Node::File { content, .. } => Some(content.clone()),
            Node::Link(_) => None,
        })
        .collect()
}

/// The files directly in or under `root` (not directories), by path.
fn files(root: &Path) -> Vec<String> {
    tree(root).into_keys().collect()
}

fn dirs(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for path in files(root) {
        let mut at = 0;
        while let Some(slash) = path[at..].find('/') {
            out.push(path[..at + slash].to_owned());
            at += slash + 1;
        }
    }
    out.sort();
    out.dedup();
    out
}

/// One random change to `root`, writing `content` where it writes.
fn mutate(rng: &mut Rng, root: &Path, content: &str) {
    let files = files(root);
    let dirs = dirs(root);
    let names = ["a", "b", "c", "d", "notes.txt", "x.md", "Makefile"];
    let fresh = || -> String {
        let depth = 1 + (content.len() % 3);
        let mut parts: Vec<String> = Vec::new();
        let mut seed = content
            .bytes()
            .fold(7u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64));
        for _ in 0..depth {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            parts.push(names[(seed >> 33) as usize % names.len()].to_owned());
        }
        parts.join("/")
    };

    match rng.below(10) {
        // Edit an existing file.
        0..=2 if !files.is_empty() => {
            let path = root.join(rng.pick(&files));
            if path.symlink_metadata().is_ok_and(|m| m.is_file()) {
                fs::write(path, content).expect("edit");
            }
        }
        // A new file somewhere, maybe in a new directory.
        0..=4 => {
            let path = root.join(fresh());
            if fs::create_dir_all(path.parent().expect("parent")).is_ok() && !path.is_dir() {
                let _ = fs::write(path, content);
            }
        }
        // Delete a file.
        5 if !files.is_empty() => {
            let _ = fs::remove_file(root.join(rng.pick(&files)));
        }
        // Rename a file.
        6 if !files.is_empty() => {
            let from = root.join(rng.pick(&files));
            let to = root.join(fresh());
            if fs::create_dir_all(to.parent().expect("parent")).is_ok() && !to.exists() {
                let _ = fs::rename(from, to);
            }
        }
        // Rename or delete a whole directory.
        7 if !dirs.is_empty() => {
            let dir = root.join(rng.pick(&dirs));
            if rng.below(2) == 0 {
                let _ = fs::remove_dir_all(dir);
            } else {
                let _ = fs::rename(&dir, dir.with_extension("moved"));
            }
        }
        // Replace a file with a directory of the same name.
        8 if !files.is_empty() => {
            let path = root.join(rng.pick(&files));
            if fs::remove_file(&path).is_ok() {
                let _ = fs::create_dir_all(&path);
                let _ = fs::write(path.join("inside"), content);
            }
        }
        // Make a file executable, or not.
        _ if !files.is_empty() => {
            let path = root.join(rng.pick(&files));
            #[cfg(unix)]
            if let Ok(meta) = path.symlink_metadata()
                && meta.is_file()
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode() ^ 0o111;
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode));
            }
            #[cfg(not(unix))]
            let _ = path;
        }
        _ => {}
    }
}

#[test]
fn random_edits_on_disconnected_devices_converge_and_lose_nothing() {
    let (_root, mut devices) = cluster(3);
    share(&devices, &[]);
    let mut rng = Rng::new(common::seed(1));

    for i in 0..30 {
        let path = devices[0].path(&format!("dir{}/file{i}.txt", i % 4));
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, format!("original {i}\n")).expect("write");
    }
    converge(&devices.iter().collect::<Vec<_>>(), Duration::from_secs(60));

    for round in 0..3 {
        for device in &mut devices {
            device.stop();
        }

        // Work on every device while none can see the others.
        let mut written = BTreeSet::new();
        for device in &devices {
            for op in 0..25 {
                let content = format!("{} round {round} op {op}\n", device.name);
                mutate(&mut rng, &device.folder, &content);
            }
            // What this device holds now that was written this round must
            // survive somewhere: at its path, or as a conflict copy.
            for content in contents(&tree(&device.folder)) {
                if String::from_utf8_lossy(&content).contains(&format!("round {round} ")) {
                    written.insert(content);
                }
            }
        }

        for device in &mut devices {
            device.start();
        }
        let merged = converge(
            &devices.iter().collect::<Vec<_>>(),
            Duration::from_secs(120),
        );
        let kept = contents(&merged);
        let lost: Vec<_> = written
            .iter()
            .filter(|content| !kept.contains(*content))
            .map(|content| String::from_utf8_lossy(content).trim().to_owned())
            .collect();
        assert!(lost.is_empty(), "round {round} lost: {lost:?}");
        eprintln!(
            "round {round}: {} files, {} conflict copies",
            merged.len(),
            merged.keys().filter(|p| p.contains("(conflict")).count()
        );
    }
}

#[test]
fn a_storm_of_concurrent_writes_converges() {
    let (_root, devices) = cluster(3);
    share(&devices, &[]);
    let mut rng = Rng::new(common::seed(42));
    let started = Instant::now();
    let mut op = 0;

    // Every device writes, renames and deletes while all are connected.
    while started.elapsed() < Duration::from_secs(12) {
        let device = &devices[rng.below(devices.len())];
        mutate(
            &mut rng,
            &device.folder,
            &format!("{} storm {op}\n", device.name),
        );
        op += 1;
        std::thread::sleep(Duration::from_millis(rng.below(40) as u64));
    }
    eprintln!("{op} operations");

    converge(
        &devices.iter().collect::<Vec<_>>(),
        Duration::from_secs(120),
    );
}

#[test]
fn crashes_mid_transfer_lose_nothing_and_delete_nothing() {
    let (_root, mut devices) = cluster(2);
    share(&devices, &[]);
    let big: Vec<u8> = (0..48_000_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
        .collect();
    fs::write(devices[0].path("big.bin"), &big).expect("write");
    for i in 0..400 {
        devices[0].write(&format!("many/{i}.txt"), &format!("small {i}\n"));
    }

    // Kill the receiver while it pulls, a few times, then the sender.
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(700));
        devices[1].kill();
        std::thread::sleep(Duration::from_millis(200));
        devices[1].start();
    }
    std::thread::sleep(Duration::from_millis(500));
    devices[0].kill();
    devices[0].start();

    let merged = converge(
        &devices.iter().collect::<Vec<_>>(),
        Duration::from_secs(120),
    );
    assert_eq!(merged.len(), 401);
    assert!(matches!(merged.get("big.bin"), Some(Node::File { content, .. }) if *content == big));

    // A crash right after edits: the edit still spreads after restart.
    devices[1].write("after-crash.txt", "made just before the crash\n");
    devices[1].kill();
    devices[1].start();
    wait("the edit made before the crash", || {
        devices[0].read("after-crash.txt").is_some()
    });
}

#[test]
fn odd_names_links_and_modes_survive_the_trip() {
    let (_root, devices) = cluster(2);
    share(&devices, &[]);
    let a = &devices[0];
    let names = [
        "spaces in name.txt",
        "ünïcödé/ファイル.txt",
        "emoji 🦀.rs",
        "decomposed-e\u{301}.txt",
        "composed-\u{e9}.txt",
        "-leading-dash",
        "#hash;semi'quote\"",
        "trailing.dot.",
        "deep/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t/u/v/w/x/y/z/file",
        &"long".repeat(60),
        "empty",
    ];
    for name in names {
        let content = if name == "empty" {
            String::new()
        } else {
            format!("{name}\n")
        };
        a.write(name, &content);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        fs::write(a.path("run.sh"), "#!/bin/sh\necho hi\n").expect("write");
        fs::set_permissions(a.path("run.sh"), fs::Permissions::from_mode(0o755)).expect("chmod");
        symlink("spaces in name.txt", a.path("relative-link")).expect("link");
        symlink("/etc/hostname", a.path("absolute-link")).expect("link");
        symlink("does/not/exist", a.path("dangling-link")).expect("link");
        symlink("loop-b", a.path("loop-a")).expect("link");
        symlink("loop-a", a.path("loop-b")).expect("link");
        fs::create_dir(a.path("real-dir")).expect("mkdir");
        a.write("real-dir/inside.txt", "inside\n");
        symlink("real-dir", a.path("dir-link")).expect("link");
    }

    let merged = converge(&devices.iter().collect::<Vec<_>>(), Duration::from_secs(60));
    for name in names {
        assert!(merged.contains_key(name), "{name} did not arrive");
    }
    #[cfg(unix)]
    {
        assert!(matches!(
            merged.get("run.sh"),
            Some(Node::File {
                executable: true,
                ..
            })
        ));
        assert_eq!(merged.get("dir-link"), Some(&Node::Link("real-dir".into())));
        assert_eq!(merged.get("loop-a"), Some(&Node::Link("loop-b".into())));
        assert!(
            !merged.contains_key("dir-link/inside.txt"),
            "links are not followed"
        );
    }

    // A case-only rename, and a file and a link swapping places.
    let b = &devices[1];
    fs::rename(b.path("empty"), b.path("EMPTY")).expect("rename");
    #[cfg(unix)]
    {
        fs::remove_file(b.path("relative-link")).expect("rm");
        b.write("relative-link", "now a file\n");
        fs::remove_file(b.path("emoji 🦀.rs")).expect("rm");
        std::os::unix::fs::symlink("run.sh", b.path("emoji 🦀.rs")).expect("link");
    }
    let merged = converge(&devices.iter().collect::<Vec<_>>(), Duration::from_secs(60));
    assert!(merged.contains_key("EMPTY") && !merged.contains_key("empty"));
    #[cfg(unix)]
    assert_eq!(
        merged.get("emoji 🦀.rs"),
        Some(&Node::Link("run.sh".into()))
    );
}

#[test]
fn a_file_replaced_by_a_directory_while_edited_elsewhere_keeps_both() {
    let (_root, mut devices) = cluster(2);
    share(&devices, &[]);
    devices[0].write("thing", "original\n");
    converge(&devices.iter().collect::<Vec<_>>(), Duration::from_secs(60));

    for device in &mut devices {
        device.stop();
    }
    fs::remove_file(devices[0].path("thing")).expect("rm");
    devices[0].write("thing/inside", "the directory's file\n");
    devices[1].write("thing", "edited while apart\n");
    for device in &mut devices {
        device.start();
    }

    let merged = converge(&devices.iter().collect::<Vec<_>>(), Duration::from_secs(60));
    let kept = contents(&merged);
    assert!(kept.contains(b"the directory's file\n".as_slice()));
    assert!(kept.contains(b"edited while apart\n".as_slice()));
}

#[test]
fn edits_that_keep_size_and_mtime_are_still_seen() {
    let (_root, devices) = cluster(2);
    share(&devices, &[]);
    let (a, b) = (&devices[0], &devices[1]);
    a.write("same-size.txt", "aaaa\n");
    converge(&devices.iter().collect::<Vec<_>>(), Duration::from_secs(60));

    // Same length, mtime put back afterwards, as some editors and `cp -p`
    // do.
    let path = a.path("same-size.txt");
    let mtime = fs::metadata(&path)
        .and_then(|m| m.modified())
        .expect("mtime");
    fs::write(&path, "bbbb\n").expect("write");
    fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(mtime))
        .expect("touch");
    a.syncer(&["rescan", "shared"]);
    wait("the same-size edit", || {
        b.read("same-size.txt").as_deref() == Some("bbbb\n")
    });
}

#[test]
fn unreadable_and_read_only_files_do_not_wedge_the_folder() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if unsafe_is_root() {
            eprintln!("running as root: permissions are not enforced, skipping");
            return;
        }
        let (_root, devices) = cluster(2);
        share(&devices, &[]);
        let (a, b) = (&devices[0], &devices[1]);
        a.write("secret.txt", "private\n");
        fs::set_permissions(a.path("secret.txt"), fs::Permissions::from_mode(0o000))
            .expect("chmod");
        a.write("fine.txt", "fine\n");
        wait("the readable file", || b.read("fine.txt").is_some());
        assert!(b.read("secret.txt").is_none());

        // A read-only file on the receiving side is replaced all the same.
        b.write("readonly.txt", "old\n");
        wait("the read-only file", || a.read("readonly.txt").is_some());
        fs::set_permissions(b.path("readonly.txt"), fs::Permissions::from_mode(0o444))
            .expect("chmod");
        a.write("readonly.txt", "new\n");
        wait("the read-only file's update", || {
            b.read("readonly.txt").as_deref() == Some("new\n")
        });
        fs::set_permissions(a.path("secret.txt"), fs::Permissions::from_mode(0o644))
            .expect("chmod");
    }
}

#[cfg(unix)]
fn unsafe_is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "0")
        .unwrap_or(false)
}

#[test]
fn deleting_the_marker_while_running_stops_the_folder_safely() {
    let (_root, devices) = cluster(2);
    share(&devices, &[]);
    let (a, b) = (&devices[0], &devices[1]);
    for i in 0..20 {
        a.write(&format!("f{i}"), "x\n");
    }
    converge(&devices.iter().collect::<Vec<_>>(), Duration::from_secs(60));

    // Someone wipes the folder, syncer's directory included.
    for entry in fs::read_dir(&a.folder).expect("list").flatten() {
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(path).expect("rm");
        } else {
            fs::remove_file(path).expect("rm");
        }
    }
    assert!(
        a.try_syncer(&["rescan", "shared"]).is_err(),
        "a scan refuses a folder without its marker"
    );
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(tree(&b.folder).len(), 20, "the wipe did not spread");
    assert!(a.status().contains("marker"), "{}", a.status());
}

#[test]
fn a_forgotten_device_gets_nothing_more() {
    let (_root, devices) = cluster(3);
    share(&devices, &[]);
    let (a, b, c) = (&devices[0], &devices[1], &devices[2]);
    a.write("before.txt", "everyone\n");
    converge(&[a, b, c], Duration::from_secs(60));

    a.syncer(&["forget", "c"]);
    wait("b to hear c was removed", || !b.status().contains("c ("));
    a.write("after.txt", "not for c\n");
    wait("b to get the new file", || b.read("after.txt").is_some());
    std::thread::sleep(Duration::from_secs(3));
    assert!(c.read("after.txt").is_none());
    // c's changes no longer reach the others either.
    c.write("from-c.txt", "ignored\n");
    std::thread::sleep(Duration::from_secs(3));
    assert!(a.read("from-c.txt").is_none());
}

#[test]
fn a_file_rewritten_while_it_is_sent_ends_up_at_its_last_version() {
    let (_root, devices) = cluster(2);
    share(&devices, &[]);
    let (a, b) = (&devices[0], &devices[1]);
    let mut last = Vec::new();

    for round in 0..6u8 {
        last = vec![round; 20_000_000];
        fs::write(a.path("moving.bin"), &last).expect("write");
        std::thread::sleep(Duration::from_millis(300));
    }
    let merged = converge(&[a, b], Duration::from_secs(120));
    assert!(
        matches!(merged.get("moving.bin"), Some(Node::File { content, .. }) if *content == last)
    );
    assert!(
        !merged.keys().any(|path| path.contains("conflict")),
        "{:?}",
        merged.keys()
    );
}
