//! Hostile file activity: random edits on devices that cannot see each
//! other, storms of concurrent writes, crashes and rewrites mid-send.
//! Every test ends with the same two checks: every device holds exactly the
//! same files, and nothing a device wrote was lost.

mod common;

use common::{
    Rng, cluster, converge, share,
    tree::{Node, files, tree},
    wait,
};
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
