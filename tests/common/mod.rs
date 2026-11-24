//! Running real `syncer` services side by side on this machine, each with
//! its own home, talking over loopback without relays or discovery.

#![allow(dead_code)]

mod device;
pub mod tree;

pub use device::Device;

use tree::{Node, difference};

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

pub const SYNCER: &str = env!("CARGO_BIN_EXE_syncer");
pub const PATIENCE: Duration = Duration::from_secs(60);

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

/// Waits until every device's folder holds the same files, and stays so for
/// a moment. Panics with the differences when it never does.
pub fn converge(devices: &[&Device], patience: Duration) -> BTreeMap<String, Node> {
    let deadline = Instant::now() + patience;
    let mut stable_since: Option<(Instant, BTreeMap<String, Node>)> = None;

    loop {
        let trees: Vec<_> = devices
            .iter()
            .map(|device| tree::tree(&device.folder))
            .collect();
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
