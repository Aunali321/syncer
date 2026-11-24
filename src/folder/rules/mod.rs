//! Which paths of a folder sync. One evaluator serves both the scanner and
//! the check on incoming entries, so the two always agree. See DESIGN.md,
//! "Rules".

mod tracked;

pub use tracked::Tracked;

use crate::folder::{MARKER, OWN_DIR};
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// The ignore file syncer reads, in `.gitignore` syntax.
pub const IGNORE_FILE: &str = ".syncerignore";

/// Whether `path` is a file whose edit changes the rules.
pub fn is_rule_file(path: &str) -> bool {
    matches!(path.rsplit('/').next(), Some(".gitignore" | IGNORE_FILE))
}

/// The ignore files of one directory.
#[derive(Default)]
struct DirRules {
    syncer: Option<Gitignore>,
    git: Option<Gitignore>,
}

/// A folder's rules as its ignore files say now. Directories' files are
/// read when first needed; make a new `Rules` to see edits to them.
pub struct Rules {
    root: PathBuf,
    /// Whether Git's ignore files apply: `.gitignore` files,
    /// `.git/info/exclude` and the user's global excludes file.
    gitignore: bool,
    /// `.git/info/exclude`, then the global excludes file.
    excludes: Vec<Gitignore>,
    tracked: Arc<Tracked>,
    dirs: Mutex<HashMap<String, Arc<DirRules>>>,
    nested: Mutex<HashMap<String, bool>>,
}

impl Rules {
    /// The rules of the folder at `root`. Git's ignore files apply when the
    /// folder is a Git checkout and `use_gitignore` is set, to every path
    /// but those `tracked` holds.
    pub fn new(root: &Path, use_gitignore: bool, tracked: Arc<Tracked>) -> Self {
        let gitignore = use_gitignore && root.join(".git").is_dir();
        let excludes = if gitignore {
            [
                Some(root.join(".git/info/exclude")),
                ignore::gitignore::gitconfig_excludes_path(),
            ]
            .into_iter()
            .flatten()
            .filter_map(|file| read(root, &file))
            .collect()
        } else {
            Vec::new()
        };

        Self {
            root: root.to_owned(),
            gitignore,
            excludes,
            tracked,
            dirs: Mutex::default(),
            nested: Mutex::default(),
        }
    }

    /// Whether Git's ignore files apply, and so what HEAD tracks matters.
    pub fn follows_git(&self) -> bool {
        self.gitignore
    }

    pub fn tracked(&self) -> &Arc<Tracked> {
        &self.tracked
    }

    /// Whether `path`, relative to the root and joined with `/`, is left out.
    /// A path inside a directory that is left out is left out too.
    pub fn excluded(&self, path: &str, is_dir: bool) -> bool {
        let parts: Vec<&str> = path.split('/').collect();

        if parts
            .first()
            .is_some_and(|first| first.eq_ignore_ascii_case(OWN_DIR))
            || parts
                .iter()
                .any(|part| crate::folder::index::is_git_name(part))
        {
            return true;
        }

        // Git's files leave out nothing HEAD tracks, nor a directory holding
        // something it tracks; under such a directory they still leave out
        // what is untracked, as Git does.
        let git = self.gitignore && !self.tracked.covers(path, is_dir);

        for depth in 1..=parts.len() {
            let sub = &parts[..depth];
            let sub_is_dir = depth < parts.len() || is_dir;

            if sub_is_dir && self.is_nested_root(&sub.join("/")) {
                return true;
            }

            if self.matched(sub, sub_is_dir, git) {
                return true;
            }
        }

        false
    }

    /// Whether the ignore files exclude `parts` itself, its parents aside.
    /// The deepest directory's files win; in one directory `.syncerignore`
    /// wins over `.gitignore`; `.git/info/exclude` and the global excludes
    /// file come last. Git's files count only when `git` is set.
    fn matched(&self, parts: &[&str], is_dir: bool, git: bool) -> bool {
        let path = parts.join("/");

        for dir_depth in (0..parts.len()).rev() {
            let dir = parts[..dir_depth].join("/");
            let rules = self.dir(&dir);
            let relative = parts[dir_depth..].join("/");
            let git_rules = rules.git.as_ref().filter(|_| git);

            for matcher in [rules.syncer.as_ref(), git_rules].into_iter().flatten() {
                match matcher.matched(&relative, is_dir) {
                    Match::Ignore(_) => return true,
                    Match::Whitelist(_) => return false,
                    Match::None => {}
                }
            }
        }

        git && self
            .excludes
            .iter()
            .map(|exclude| exclude.matched(&path, is_dir))
            .find(|matched| !matched.is_none())
            .is_some_and(|matched| matched.is_ignore())
    }

    fn dir(&self, dir: &str) -> Arc<DirRules> {
        if let Some(rules) = crate::util::lock(&self.dirs).get(dir) {
            return rules.clone();
        }

        let path = if dir.is_empty() {
            self.root.clone()
        } else {
            self.root.join(dir)
        };
        let rules = Arc::new(DirRules {
            syncer: read(&path, &path.join(IGNORE_FILE)),
            git: self
                .gitignore
                .then(|| read(&path, &path.join(".gitignore")))
                .flatten(),
        });
        crate::util::lock(&self.dirs).insert(dir.to_owned(), rules.clone());
        rules
    }

    /// Whether the directory `dir` is the root of something synced on its
    /// own: a submodule or nested clone, which stays on its machine, or
    /// another syncer folder.
    fn is_nested_root(&self, dir: &str) -> bool {
        if let Some(&nested) = crate::util::lock(&self.nested).get(dir) {
            return nested;
        }

        let at = self.root.join(dir);
        let nested = std::fs::symlink_metadata(at.join(".git")).is_ok()
            || at.join(OWN_DIR).join(MARKER).is_file();
        crate::util::lock(&self.nested).insert(dir.to_owned(), nested);
        nested
    }
}

/// The patterns in `file`, matched relative to `dir`; `None` when there is no
/// such file. Lines that do not parse are skipped with a warning.
fn read(dir: &Path, file: &Path) -> Option<Gitignore> {
    if !file.is_file() {
        return None;
    }

    let mut builder = GitignoreBuilder::new(dir);

    if let Some(e) = builder.add(file) {
        tracing::warn!("{}: {e}", file.display());
    }

    match builder.build() {
        Ok(matcher) if !matcher.is_empty() => Some(matcher),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("{}: {e}", file.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn folder(git: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp dir");
        if git {
            fs::create_dir_all(dir.path().join(".git/info")).expect("git dir");
        }
        dir
    }

    #[test]
    fn git_and_own_folders_are_always_left_out() {
        let dir = folder(false);
        let rules = Rules::new(dir.path(), true, Default::default());

        assert!(rules.excluded(".git", true));
        assert!(rules.excluded("sub/.git/config", false));
        assert!(rules.excluded(".syncer/folder", false));
        assert!(!rules.excluded("src/.syncer", false));
        assert!(!rules.excluded("src/main.rs", false));
    }

    #[test]
    fn gitignore_applies_in_git_folders_only() {
        for git in [true, false] {
            let dir = folder(git);
            fs::write(dir.path().join(".gitignore"), "target/\n*.log\n").expect("write");
            let rules = Rules::new(dir.path(), true, Default::default());

            assert_eq!(rules.excluded("target", true), git);
            assert_eq!(rules.excluded("target/debug/app", false), git);
            assert_eq!(rules.excluded("a/b.log", false), git);
            assert!(!rules.excluded("src/lib.rs", false));
        }
    }

    #[test]
    fn syncerignore_can_bring_back_what_gitignore_leaves_out() {
        let dir = folder(true);
        fs::write(dir.path().join(".gitignore"), ".env\nbuild/\n").expect("write");
        fs::write(dir.path().join(IGNORE_FILE), "!.env\nnotes/\n").expect("write");
        let rules = Rules::new(dir.path(), true, Default::default());

        assert!(!rules.excluded(".env", false));
        assert!(rules.excluded("build/x", false));
        assert!(rules.excluded("notes/today.md", false));
    }

    #[test]
    fn deeper_files_win_and_excluded_directories_stay_excluded() {
        let dir = folder(true);
        fs::create_dir_all(dir.path().join("web")).expect("mkdir");
        fs::write(dir.path().join(".gitignore"), "*.gen\ncache/\n").expect("write");
        fs::write(dir.path().join("web/.gitignore"), "!keep.gen\n!cache/\n").expect("write");
        let rules = Rules::new(dir.path(), true, Default::default());

        assert!(rules.excluded("a.gen", false));
        assert!(!rules.excluded("web/keep.gen", false));
        assert!(rules.excluded("cache/x", false));
    }

    #[test]
    fn info_exclude_applies_and_gitignore_can_be_turned_off() {
        let dir = folder(true);
        fs::write(dir.path().join(".git/info/exclude"), "scratch/\n").expect("write");
        fs::write(dir.path().join(".gitignore"), "*.o\n").expect("write");

        assert!(Rules::new(dir.path(), true, Default::default()).excluded("scratch/x", false));
        let off = Rules::new(dir.path(), false, Default::default());
        assert!(!off.excluded("scratch/x", false));
        assert!(!off.excluded("main.o", false));
    }

    #[test]
    fn nested_repositories_stay_on_their_machine() {
        let dir = folder(true);
        fs::create_dir_all(dir.path().join("vendor/lib/.git")).expect("mkdir");
        let rules = Rules::new(dir.path(), true, Default::default());

        assert!(rules.excluded("vendor/lib", true));
        assert!(rules.excluded("vendor/lib/src/a.c", false));
        assert!(!rules.excluded("vendor/other.c", false));
    }

    #[test]
    fn nested_folders_stay_with_their_own_folder() {
        let dir = folder(false);
        fs::create_dir_all(dir.path().join("repo/.syncer")).expect("mkdir");
        fs::write(dir.path().join("repo/.syncer/folder"), "id").expect("write");
        fs::create_dir_all(dir.path().join("notes/.syncer")).expect("mkdir");
        let rules = Rules::new(dir.path(), true, Default::default());

        assert!(rules.excluded("repo/src/a.rs", false));
        assert!(
            !rules.excluded("notes/today.md", false),
            "a .syncer directory without a marker is not a folder"
        );
    }

    #[test]
    fn git_ignores_nothing_head_tracks() {
        let dir = folder(true);
        fs::write(dir.path().join(".gitignore"), "*.md\ntarget/\n").expect("write");
        let tracked = Tracked::new(
            None,
            ["docs/kept.md", "target/pinned.bin"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );
        let rules = Rules::new(dir.path(), true, Arc::new(tracked));

        assert!(!rules.excluded("docs/kept.md", false));
        assert!(rules.excluded("docs/other.md", false));
        assert!(!rules.excluded("target", true), "it holds a tracked file");
        assert!(!rules.excluded("target/pinned.bin", false));
        assert!(
            rules.excluded("target/debug/app", false),
            "untracked files under it stay out"
        );
    }

    #[test]
    fn syncerignore_applies_to_tracked_paths_too() {
        let dir = folder(true);
        fs::write(dir.path().join(IGNORE_FILE), "secret.txt\n").expect("write");
        let tracked = Tracked::new(None, ["secret.txt".to_owned()].into());
        let rules = Rules::new(dir.path(), true, Arc::new(tracked));

        assert!(rules.excluded("secret.txt", false));
    }
}
