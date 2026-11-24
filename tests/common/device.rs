//! One device: a real `syncer run` with its own home and folder.

use super::{SYNCER, git, tree, try_git, wait};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

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
        self.try_syncer(&["status", "--all"]).unwrap_or_default()
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
        tree::tree(&self.folder)
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
