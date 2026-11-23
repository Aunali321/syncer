//! Running real `syncer` services side by side on this machine, each with
//! its own home, talking over loopback without relays or discovery.

#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub const SYNCER: &str = env!("CARGO_BIN_EXE_syncer");
pub const PATIENCE: Duration = Duration::from_secs(60);

pub struct Device {
    pub name: String,
    pub home: PathBuf,
    pub folder: PathBuf,
    pub log: PathBuf,
    process: Option<Child>,
}

impl Device {
    pub fn new(root: &Path, name: &str) -> Self {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("a device directory");
        Self {
            name: name.to_owned(),
            home: dir.join("home"),
            folder: dir.join("project"),
            log: dir.join("log"),
            process: None,
        }
    }

    pub fn start(&mut self) {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .expect("a log");
        let child = Command::new(SYNCER)
            .arg("--home")
            .arg(&self.home)
            .args(["run", "--no-relays", "--no-mdns"])
            .env("HOSTNAME", &self.name)
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

    pub fn running(&self) -> bool {
        self.process.is_some()
    }

    /// Stops the service the way a user does.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = self.try_syncer(&["stop"]);
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Kills the service without warning, as a crash or power cut does.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn try_syncer(&self, args: &[&str]) -> Result<String, String> {
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

    pub fn syncer(&self, args: &[&str]) -> String {
        self.try_syncer(args)
            .unwrap_or_else(|e| panic!("{}: syncer {args:?} failed: {e}", self.name))
    }

    pub fn status(&self) -> String {
        self.try_syncer(&["status"]).unwrap_or_default()
    }

    pub fn git(&self, args: &[&str]) -> String {
        git(&self.folder, args)
    }

    pub fn try_git(&self, args: &[&str]) -> Result<String, String> {
        try_git(&self.folder, args)
    }

    pub fn path(&self, path: &str) -> PathBuf {
        self.folder.join(path)
    }

    pub fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.path(path)).ok()
    }

    pub fn write(&self, path: &str, text: &str) {
        let path = self.path(path);
        fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        fs::write(path, text).expect("write");
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"]).trim().to_owned()
    }

    pub fn clean(&self) -> bool {
        self.git(&["status", "--porcelain"]).trim().is_empty()
    }

    pub fn conflict_copies(&self) -> Vec<String> {
        tree(&self.folder)
            .into_keys()
            .filter(|path| path.contains(" (conflict "))
            .collect()
    }

    pub fn conflict_refs(&self) -> String {
        self.git(&["for-each-ref", "--format=%(refname)", "refs/syncer"])
    }

    /// What `git` says about branches, tags and HEAD, for comparing devices.
    pub fn git_view(&self) -> String {
        let refs = self.git(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/tags",
        ]);
        let head = self
            .try_git(&["symbolic-ref", "-q", "HEAD"])
            .unwrap_or_else(|_| self.git(&["rev-parse", "HEAD"]));
        format!("HEAD {}{refs}", head)
    }

    pub fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.kill();
    }
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    try_git(dir, args).unwrap_or_else(|e| panic!("git {args:?} in {}: {e}", dir.display()))
}

pub fn try_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_EDITOR", "true")
        .output()
        .expect("git runs");
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

pub fn wait(what: &str, ready: impl FnMut() -> bool) {
    wait_for(what, PATIENCE, ready);
}

pub fn wait_for(what: &str, patience: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + patience;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

pub fn all(devices: &[&Device], what: &str, mut ready: impl FnMut(&Device) -> bool) {
    wait(what, || devices.iter().all(|device| ready(device)));
}

/// One entry of a folder as the file system shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    File { content: Vec<u8>, executable: bool },
    Link(String),
}

/// Every file and symlink under `root`, by `/`-separated path, leaving out
/// syncer's own directory and `.git` directories.
pub fn tree(root: &Path) -> BTreeMap<String, Node> {
    let mut out = BTreeMap::new();
    walk(root, "", &mut out);
    out
}

fn walk(root: &Path, dir: &str, out: &mut BTreeMap<String, Node>) {
    let full = if dir.is_empty() {
        root.to_owned()
    } else {
        root.join(dir)
    };
    let Ok(entries) = fs::read_dir(&full) else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if dir.is_empty() {
            name.clone()
        } else {
            format!("{dir}/{name}")
        };
        if (dir.is_empty() && name == ".syncer") || name == ".git" {
            continue;
        }
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() {
            let target = fs::read_link(entry.path()).expect("a link");
            out.insert(path, Node::Link(target.to_string_lossy().into_owned()));
        } else if meta.is_dir() {
            walk(root, &path, out);
        } else {
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            out.insert(
                path,
                Node::File {
                    content: fs::read(entry.path()).unwrap_or_default(),
                    executable,
                },
            );
        }
    }
}

/// The paths where two trees differ, for a failure message.
pub fn difference(a: &BTreeMap<String, Node>, b: &BTreeMap<String, Node>) -> Vec<String> {
    let mut paths: Vec<String> = a
        .keys()
        .chain(b.keys())
        .filter(|path| a.get(*path) != b.get(*path))
        .cloned()
        .collect();
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|path| {
            let show = |node: Option<&Node>| match node {
                None => "missing".to_owned(),
                Some(Node::Link(target)) => format!("link to {target}"),
                Some(Node::File {
                    content,
                    executable,
                }) => format!(
                    "{:?}{}",
                    String::from_utf8_lossy(&content[..content.len().min(40)]),
                    if *executable { " (exec)" } else { "" }
                ),
            };
            format!("{path}: {} vs {}", show(a.get(&path)), show(b.get(&path)))
        })
        .collect()
}

/// Waits until every device's folder holds the same files, and stays so for
/// a moment. Panics with the differences when it never does.
pub fn converge(devices: &[&Device], patience: Duration) -> BTreeMap<String, Node> {
    let deadline = Instant::now() + patience;
    let mut stable_since: Option<(Instant, BTreeMap<String, Node>)> = None;

    loop {
        let trees: Vec<_> = devices.iter().map(|device| tree(&device.folder)).collect();
        let same = trees.windows(2).all(|pair| pair[0] == pair[1]);

        if same {
            match &stable_since {
                Some((since, seen)) if *seen == trees[0] => {
                    if since.elapsed() > Duration::from_secs(3) {
                        return trees[0].clone();
                    }
                }
                _ => stable_since = Some((Instant::now(), trees[0].clone())),
            }
        } else {
            stable_since = None;
        }

        if Instant::now() > deadline {
            let mut report = String::new();
            for (device, tree) in devices.iter().zip(&trees).skip(1) {
                report.push_str(&format!(
                    "\n{} vs {}:\n  {}",
                    devices[0].name,
                    device.name,
                    difference(&trees[0], tree).join("\n  ")
                ));
            }
            for device in devices {
                report.push_str(&format!(
                    "\n--- {} status:\n{}",
                    device.name,
                    device.status()
                ));
            }
            panic!("the devices did not converge:{report}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `n` running devices that trust each other, in a temporary directory kept
/// when `SYNCER_TEST_KEEP` is set.
pub fn cluster(n: usize) -> (tempfile::TempDir, Vec<Device>) {
    let root = tempfile::Builder::new()
        .prefix("syncer-test")
        .disable_cleanup(std::env::var_os("SYNCER_TEST_KEEP").is_some())
        .tempdir()
        .expect("a temp dir");
    eprintln!("devices in {}", root.path().display());
    let names = ["a", "b", "c", "d", "e"];
    let mut devices: Vec<Device> = names[..n]
        .iter()
        .map(|name| Device::new(root.path(), name))
        .collect();
    for device in &mut devices {
        device.start();
    }
    for device in &devices[1..] {
        let ticket = devices[0].syncer(&["invite"]);
        device.syncer(&["join", ticket.trim()]);
    }
    let count = n - 1;
    all(
        &devices.iter().collect::<Vec<_>>(),
        "everyone to know everyone",
        |d| d.status().matches("): connected").count() == count,
    );
    (root, devices)
}

/// Shares the first device's folder (made if missing) and has every other
/// device accept it. `extra` goes to `syncer add`.
pub fn share(devices: &[Device], extra: &[&str]) {
    fs::create_dir_all(&devices[0].folder).expect("mkdir");
    let mut args = vec![
        "add",
        devices[0].folder.to_str().expect("utf-8"),
        "--label",
        "shared",
    ];
    args.extend_from_slice(extra);
    devices[0].syncer(&args);
    for device in &devices[1..] {
        wait("the offer", || device.status().contains("shared ["));
        device.syncer(&["accept", "shared", device.folder.to_str().expect("utf-8")]);
    }
    all(
        &devices.iter().collect::<Vec<_>>(),
        "the folder to start everywhere",
        |d| d.folder.join(".syncer/folder").exists(),
    );
}

/// The seed from `SYNCER_SEED`, else `default`: rerun a failure with the
/// seed it printed.
pub fn seed(default: u64) -> u64 {
    let seed = std::env::var("SYNCER_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(default);
    eprintln!("seed {seed}");
    seed
}

/// A small deterministic random number generator for repeatable fuzzing.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}
