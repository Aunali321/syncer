//! Which paths of a folder sync. One evaluator serves both the scanner and
//! the check on incoming entries, so the two always agree. See DESIGN.md,
//! "Rules".

use crate::folder::OWN_DIR;
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
    /// Whether `.gitignore` files and `.git/info/exclude` apply.
    gitignore: bool,
    exclude: Option<Gitignore>,
    dirs: Mutex<HashMap<String, Arc<DirRules>>>,
    nested_git: Mutex<HashMap<String, bool>>,
}

impl Rules {
    /// The rules of the folder at `root`. `.gitignore` applies when the folder
    /// is a Git checkout and `use_gitignore` is set.
    pub fn new(root: &Path, use_gitignore: bool) -> Self {
        let gitignore = use_gitignore && root.join(".git").is_dir();
        let exclude = gitignore
            .then(|| read(root, &root.join(".git/info/exclude")))
            .flatten();

        Self {
            root: root.to_owned(),
            gitignore,
            exclude,
            dirs: Mutex::default(),
            nested_git: Mutex::default(),
        }
    }

    /// Whether `path`, relative to the root and joined with `/`, is left out.
    /// A path inside a directory that is left out is left out too.
    pub fn excluded(&self, path: &str, is_dir: bool) -> bool {
        let parts: Vec<&str> = path.split('/').collect();

        if parts.first().is_some_and(|first| first.eq_ignore_ascii_case(OWN_DIR))
            || parts.iter().any(|part| part.eq_ignore_ascii_case(".git"))
        {
            return true;
        }

        for depth in 1..=parts.len() {
            let sub = &parts[..depth];
            let sub_is_dir = depth < parts.len() || is_dir;

            if sub_is_dir && self.is_nested_repo(&sub.join("/")) {
                return true;
            }

            if self.matched(sub, sub_is_dir) {
                return true;
            }
        }

        false
    }

    /// Whether the ignore files exclude `parts` itself, its parents aside.
    /// The deepest directory's files win; in one directory `.syncerignore`
    /// wins over `.gitignore`; `.git/info/exclude` comes last.
    fn matched(&self, parts: &[&str], is_dir: bool) -> bool {
        for dir_depth in (0..parts.len()).rev() {
            let dir = parts[..dir_depth].join("/");
            let rules = self.dir(&dir);
            let relative = parts[dir_depth..].join("/");

            for matcher in [&rules.syncer, &rules.git].into_iter().flatten() {
                match matcher.matched(&relative, is_dir) {
                    Match::Ignore(_) => return true,
                    Match::Whitelist(_) => return false,
                    Match::None => {}
                }
            }
        }

        self.exclude
            .as_ref()
            .is_some_and(|exclude| exclude.matched(parts.join("/"), is_dir).is_ignore())
    }

    fn dir(&self, dir: &str) -> Arc<DirRules> {
        if let Some(rules) = crate::lock(&self.dirs).get(dir) {
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
        crate::lock(&self.dirs).insert(dir.to_owned(), rules.clone());
        rules
    }

    /// Whether the directory `dir` holds its own repository: a submodule or a
    /// nested clone, which stays on its machine.
    fn is_nested_repo(&self, dir: &str) -> bool {
        if let Some(&nested) = crate::lock(&self.nested_git).get(dir) {
            return nested;
        }

        let nested = std::fs::symlink_metadata(self.root.join(dir).join(".git")).is_ok();
        crate::lock(&self.nested_git).insert(dir.to_owned(), nested);
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
        let rules = Rules::new(dir.path(), true);

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
            let rules = Rules::new(dir.path(), true);

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
        let rules = Rules::new(dir.path(), true);

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
        let rules = Rules::new(dir.path(), true);

        assert!(rules.excluded("a.gen", false));
        assert!(!rules.excluded("web/keep.gen", false));
        assert!(rules.excluded("cache/x", false));
    }

    #[test]
    fn info_exclude_applies_and_gitignore_can_be_turned_off() {
        let dir = folder(true);
        fs::write(dir.path().join(".git/info/exclude"), "scratch/\n").expect("write");
        fs::write(dir.path().join(".gitignore"), "*.o\n").expect("write");

        assert!(Rules::new(dir.path(), true).excluded("scratch/x", false));
        let off = Rules::new(dir.path(), false);
        assert!(!off.excluded("scratch/x", false));
        assert!(!off.excluded("main.o", false));
    }

    #[test]
    fn nested_repositories_stay_on_their_machine() {
        let dir = folder(true);
        fs::create_dir_all(dir.path().join("vendor/lib/.git")).expect("mkdir");
        let rules = Rules::new(dir.path(), true);

        assert!(rules.excluded("vendor/lib", true));
        assert!(rules.excluded("vendor/lib/src/a.c", false));
        assert!(!rules.excluded("vendor/other.c", false));
    }
}
