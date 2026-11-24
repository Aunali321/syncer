//! Edge cases one at a time: odd names, links and modes, files that cannot
//! be read, a lost folder marker, a forgotten device, names that cannot sync,
//! a second service on one home, and a folder inside another.

mod common;

use common::{
    Device, SYNCER, all, cluster, converge, share,
    tree::{Node, files, tree},
    wait,
};
use std::{fs, time::Duration};

#[test]
fn odd_names_links_and_modes_survive_the_trip() {
    let (_root, devices) = cluster(2);
    share(&devices, &[]);
    let a = &devices[0];
    let long = "long".repeat(60);
    let mut names = vec![
        "spaces in name.txt",
        "ünïcödé/ファイル.txt",
        "emoji 🦀.rs",
        "decomposed-e\u{301}.txt",
        "composed-\u{e9}.txt",
        "-leading-dash",
        "deep/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t/u/v/w/x/y/z/file",
        long.as_str(),
        "empty",
    ];
    // Windows takes neither a quote in a name nor a dot at its end.
    if cfg!(unix) {
        names.extend(["#hash;semi'quote\"", "trailing.dot."]);
    }
    for &name in &names {
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
    for &name in &names {
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
    assert!(a.status().contains("folder not found"), "{}", a.status());
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
#[cfg_attr(windows, ignore = "Windows cannot make these names")]
fn names_that_cannot_sync_stay_where_they_are_and_say_so() {
    let (_root, devices) = cluster(2);
    share(&devices, &[]);
    let (a, b) = (&devices[0], &devices[1]);
    for name in ["new\nline.txt", "back\\slash.txt", "tab\there.txt"] {
        a.write(name, "stays\n");
    }
    a.write("fine.txt", "goes\n");

    wait("the file that can sync", || b.read("fine.txt").is_some());
    let status = a.status();
    assert!(
        status.contains("3 files can't sync (invalid name)"),
        "{status}"
    );
    for shown in ["new\\nline.txt: ", "back\\slash.txt: ", "tab\\there.txt: "] {
        assert!(status.contains(shown), "{status}");
    }
    assert_eq!(files(&b.folder), ["fine.txt"]);
    assert!(a.read("new\nline.txt").is_some(), "nothing is deleted");
}

#[test]
fn a_second_service_on_one_home_is_refused() {
    let (_root, devices) = cluster(1);
    let a = &devices[0];
    let second = std::process::Command::new(SYNCER)
        .arg("--home")
        .arg(&a.home)
        .args(["run", "--no-relays", "--no-mdns"])
        .output()
        .expect("syncer runs");

    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));
    assert!(
        a.try_syncer(&["status"]).is_ok(),
        "the first one still answers"
    );
}

#[test]
fn a_folder_inside_another_syncs_as_its_own() {
    let (root, devices) = cluster(2);
    let (a, b) = (&devices[0], &devices[1]);
    let holder = a.folder.clone();
    a.write("notes/today.md", "notes\n");
    a.write("repo/src/main.rs", "fn main() {}\n");
    common::git(&holder.join("repo"), &["init", "-q", "-b", "main"]);
    common::git(&holder.join("repo"), &["add", "-A"]);
    common::git(&holder.join("repo"), &["commit", "-q", "-m", "init"]);

    let path =
        |device: &Device, sub: &str| device.folder.join(sub).to_str().expect("utf-8").to_owned();
    a.syncer(&["add", &path(a, ""), "--label", "holder"]);
    a.syncer(&["add", &path(a, "repo"), "--label", "repo"]);
    for label in ["holder", "repo"] {
        wait("the offers", || b.status().contains(&format!("{label} [")));
    }
    let outer = root.path().join("b-holder");
    b.syncer(&["accept", "holder", outer.to_str().expect("utf-8")]);
    b.syncer(&[
        "accept",
        "repo",
        outer.join("repo").to_str().expect("utf-8"),
    ]);

    all(&[b], "both folders", |_| {
        outer.join("notes/today.md").is_file() && outer.join("repo/src/main.rs").is_file()
    });
    assert!(
        outer.join("repo/.git").is_dir(),
        "the inner one is its own repository"
    );
    let holder_status = b.status();
    assert!(!holder_status.contains("stopped"), "{holder_status}");
}

#[cfg(unix)]
fn unsafe_is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "0")
        .unwrap_or(false)
}
