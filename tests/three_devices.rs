//! Three devices on this machine, each a real `syncer run` process with its
//! own home, talking over loopback without relays: pairing, a shared Git
//! folder, and the cases that break byte-level sync tools.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const SYNCER: &str = env!("CARGO_BIN_EXE_syncer");
const PATIENCE: Duration = Duration::from_secs(45);

struct Device {
    name: &'static str,
    home: PathBuf,
    folder: PathBuf,
    log: PathBuf,
    process: Option<Child>,
}

impl Device {
    fn new(root: &Path, name: &'static str) -> Self {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("a device directory");
        Self {
            name,
            home: dir.join("home"),
            folder: dir.join("project"),
            log: dir.join("log"),
            process: None,
        }
    }

    fn start(&mut self) {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .expect("a log");
        let child = Command::new(SYNCER)
            .args(["--home"])
            .arg(&self.home)
            .args(["run", "--no-relays", "--no-mdns"])
            .env("HOSTNAME", self.name)
            .env("SYNCER_LOG", "syncer=trace,iroh=warn,noq=warn")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("syncer starts");
        self.process = Some(child);
        wait(&format!("{} to listen", self.name), || {
            self.try_syncer(&["status"]).is_ok()
        });
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = self.try_syncer(&["stop"]);
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn try_syncer(&self, args: &[&str]) -> Result<String, String> {
        let output = Command::new(SYNCER)
            .arg("--home")
            .arg(&self.home)
            .args(args)
            .output()
            .expect("syncer runs");
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }

    fn syncer(&self, args: &[&str]) -> String {
        self.try_syncer(args)
            .unwrap_or_else(|e| panic!("{}: syncer {args:?} failed: {e}", self.name))
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.folder, args)
    }

    fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.folder.join(path)).ok()
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.folder.join(path);
        fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        fs::write(path, text).expect("write");
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"]).trim().to_owned()
    }

    fn clean(&self) -> bool {
        self.git(&["status", "--porcelain"]).trim().is_empty()
    }

    fn conflict_copies(&self) -> Vec<String> {
        let mut copies: Vec<String> = fs::read_dir(&self.folder)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.contains(" (conflict "))
                    .collect()
            })
            .unwrap_or_default();
        copies.sort();
        copies
    }

    fn conflict_refs(&self) -> String {
        self.git(&["for-each-ref", "--format=%(refname)", "refs/syncer"])
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn wait(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

fn all(devices: &[&Device], what: &str, mut ready: impl FnMut(&Device) -> bool) {
    wait(what, || devices.iter().all(|device| ready(device)));
}

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
    a.git(&["add", "-A"]);
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
        b.syncer(&["status"]).contains("project [")
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
    assert!(
        c.syncer(&["status"])
            .contains("git merge refs/syncer/conflicts/")
    );

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
            .contains("lost its .syncer/folder marker")
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
