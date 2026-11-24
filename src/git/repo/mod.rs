//! A Git checkout, driven through the `git` command. Syncer never writes
//! inside `.git` itself. Everything here blocks.

mod oid;
mod refs;
mod transfer;

pub use oid::{HeadTarget, Oid};
pub use transfer::{drain_stderr, finish};

use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
};

/// The prefix of branch refs.
pub const HEADS: &str = "refs/heads/";
/// The prefix of tag refs.
pub const TAGS: &str = "refs/tags/";
/// Where diverged peer tips are kept, as `refs/syncer/<device>/<branch>`:
/// `git merge syncer/<device>/<branch>` finds them.
pub const CONFLICTS: &str = "refs/syncer/";

/// Files whose presence means a Git operation is under way, during which
/// HEAD and the branches are not what the user means them to be.
const BUSY: [&str; 8] = [
    "index.lock",
    "HEAD.lock",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "rebase-merge",
    "rebase-apply",
];

/// A checkout's repository.
#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// The repository whose work tree is `root`, when `root/.git` is one.
    /// Linked worktrees and submodules (a `.git` file) are not synced.
    pub fn open(root: &Path) -> Option<Self> {
        root.join(".git").is_dir().then(|| Self {
            root: root.to_owned(),
        })
    }

    /// Makes a new repository at `root` whose HEAD names `branch`.
    pub fn init(root: &Path, branch: &str) -> Result<Self> {
        let repo = Self {
            root: root.to_owned(),
        };
        repo.run(["init", "--quiet"])?;
        repo.run(["symbolic-ref", "HEAD", &format!("{HEADS}{branch}")])?;
        Ok(repo)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn git_dir(&self) -> PathBuf {
        self.root.join(".git")
    }

    /// A `git` command in this checkout that never prompts and reads no
    /// environment that could point it at another repository.
    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .stdin(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command
    }

    fn output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        self.command()
            .args(&args)
            .output()
            .with_context(|| format!("running git {}", show(&args)))
    }

    /// Runs git and gives its standard output, failing with its message when
    /// it fails.
    fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        let output = self.output(&args)?;

        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                show(&args),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        String::from_utf8(output.stdout).context("git printed something that is not UTF-8")
    }

    /// Whether a merge, rebase, bisect or other operation is under way, or
    /// another git holds the index.
    pub fn busy(&self) -> Option<&'static str> {
        let dir = self.git_dir();
        BUSY.into_iter().find(|name| dir.join(name).exists())
    }

    /// The repository's config as `key=value` pairs, multi-valued keys
    /// repeated, from `.git/config` only.
    pub fn local_config(&self) -> Result<Vec<(String, String)>> {
        let listed = self.run(["config", "--local", "--null", "--list"])?;
        Ok(listed
            .split('\0')
            .filter(|item| !item.is_empty())
            .map(|item| {
                let (key, value) = item.split_once('\n').unwrap_or((item, ""));
                (key.to_owned(), value.to_owned())
            })
            .collect())
    }

    pub fn add_config(&self, key: &str, value: &str) -> Result<()> {
        self.run(["config", "--local", "--add", key, value])?;
        Ok(())
    }

    /// Adds `pattern` to `.git/info/exclude` unless a line already says it.
    pub fn exclude(&self, pattern: &str) -> Result<()> {
        let path = self.git_dir().join("info").join("exclude");
        let current = std::fs::read_to_string(&path).unwrap_or_default();

        if current.lines().any(|line| line.trim() == pattern) {
            return Ok(());
        }

        std::fs::create_dir_all(path.parent().context("a path with no parent")?)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        if !current.is_empty() && !current.ends_with('\n') {
            file.write_all(b"\n")?;
        }
        writeln!(file, "{pattern}")?;
        Ok(())
    }
}

fn show(args: &[std::ffi::OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A repository in a temporary directory with one commit on `main`.
    pub fn repo() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let repo = Repo::init(dir.path(), "main").expect("git init");
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
        ] {
            repo.run(["config", key, value]).expect("git config");
        }
        commit(&repo, "first");
        (dir, repo)
    }

    /// Commits a change to `file.txt` on the current branch.
    pub fn commit(repo: &Repo, text: &str) -> Oid {
        std::fs::write(repo.root().join("file.txt"), text).expect("write");
        repo.run(["add", "file.txt"]).expect("git add");
        repo.run(["commit", "--quiet", "-m", text])
            .expect("git commit");
        repo.head_commit().expect("HEAD").expect("a commit")
    }

    pub fn git(repo: &Repo, args: &[&str]) -> String {
        repo.run(args).expect("git runs")
    }
}
