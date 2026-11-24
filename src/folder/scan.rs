//! Walking a folder: the files and symlinks that sync, with their stat data.
//! What changed is decided by the caller against what it last saw. Everything
//! here blocks.

use crate::folder::{
    disk::{self, Folding, Stat},
    index::path_problem,
    rules::Rules,
};
use std::{
    collections::BTreeMap,
    fs, io,
    io::ErrorKind,
    path::{Path, PathBuf},
};

/// What a walk found.
#[derive(Debug, Default)]
pub struct Walked {
    /// Every syncable path in the walked scope, by folder path.
    pub files: BTreeMap<String, Stat>,
    /// Paths that could not be read, with why. Nothing under them counts as
    /// deleted.
    pub unreadable: BTreeMap<String, String>,
    /// Paths whose names cannot sync, with why; a directory's contents with
    /// it.
    pub refused: BTreeMap<String, String>,
}

/// What part of a folder to walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    All,
    /// These paths and, for directories, everything under them.
    Paths(Vec<String>),
}

impl Scope {
    /// Whether `path` lies in this scope.
    pub fn covers(&self, path: &str) -> bool {
        match self {
            Self::All => true,
            Self::Paths(paths) => paths.iter().any(|scoped| within(path, scoped)),
        }
    }
}

/// Whether `path` is `dir` or inside it.
pub fn within(path: &str, dir: &str) -> bool {
    // The root holds everything.
    dir.is_empty()
        || path == dir
        || (path.starts_with(dir) && path.as_bytes().get(dir.len()) == Some(&b'/'))
}

/// Walks `scope` of the folder at `root`. On a file system that folds names
/// (`folding`), a path is only found where it is spelled exactly so.
pub fn walk(root: &Path, rules: &Rules, scope: &Scope, folding: Folding) -> Walked {
    let mut walked = Walked::default();

    match scope {
        Scope::All => walk_dir(root, "", rules, &mut walked),
        Scope::Paths(paths) => {
            for path in paths {
                if rules.excluded(path, false) {
                    continue;
                }
                if let Some(why) = path_problem(path) {
                    walked.refused.insert(path.clone(), why.to_owned());
                    continue;
                }

                let full = disk::join(root, path);
                let found = match spelled(root, path, folding) {
                    Ok(true) => fs::symlink_metadata(&full),
                    Ok(false) => Err(ErrorKind::NotFound.into()),
                    Err(e) => Err(e),
                };
                match found {
                    Ok(meta) if meta.is_dir() => {
                        if !rules.excluded(path, true) {
                            walk_dir(root, path, rules, &mut walked);
                        }
                    }
                    Ok(meta) => {
                        if let Some(stat) = Stat::of(&meta) {
                            walked.files.insert(path.clone(), stat);
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => {
                        walked.unreadable.insert(path.clone(), e.to_string());
                    }
                }
            }
        }
    }

    walked
}

/// Whether every part of `path` exists spelled exactly so. Where names fold,
/// looking a path up also finds other spellings of it: only a directory's
/// listing tells which spelling is there.
fn spelled(root: &Path, path: &str, folding: Folding) -> io::Result<bool> {
    if folding == Folding::default() {
        return Ok(true);
    }
    let mut dir = PathBuf::from(root);
    for part in path.split('/') {
        let listed = fs::read_dir(&dir)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        if !listed.iter().any(|name| name == part) {
            return Ok(false);
        }
        dir.push(part);
    }
    Ok(true)
}

fn walk_dir(root: &Path, dir: &str, rules: &Rules, walked: &mut Walked) {
    let full = disk::join(root, dir);
    let full = if dir.is_empty() {
        root.to_owned()
    } else {
        full
    };
    let entries = match fs::read_dir(&full) {
        Ok(entries) => entries,
        Err(e) => {
            walked.unreadable.insert(dir.to_owned(), e.to_string());
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                walked.unreadable.insert(dir.to_owned(), e.to_string());
                continue;
            }
        };
        let file_name = entry.file_name();
        let (name, utf8) = match file_name.to_str() {
            Some(name) => (name.to_owned(), true),
            None => (file_name.to_string_lossy().into_owned(), false),
        };
        let path = if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        };
        if !utf8 {
            walked.refused.insert(path, "invalid UTF-8 name".to_owned());
            continue;
        }
        let meta = match entry.path().symlink_metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                walked.unreadable.insert(path, e.to_string());
                continue;
            }
        };
        let is_dir = meta.is_dir();

        if rules.excluded(&path, is_dir) {
            continue;
        }
        if let Some(why) = path_problem(&path) {
            walked.refused.insert(path, why.to_owned());
            continue;
        }

        if is_dir {
            walk_dir(root, &path, rules, walked);
        } else if let Some(stat) = Stat::of(&meta) {
            walked.files.insert(path, stat);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_what_the_rules_keep() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        fs::create_dir_all(root.join(".git")).expect("mkdir");
        fs::create_dir_all(root.join("src/deep")).expect("mkdir");
        fs::create_dir_all(root.join("target")).expect("mkdir");
        fs::create_dir_all(root.join(".syncer")).expect("mkdir");
        fs::write(root.join(".gitignore"), "target/\n").expect("write");
        fs::write(root.join("src/deep/a.rs"), "a").expect("write");
        fs::write(root.join("target/out"), "o").expect("write");
        fs::write(root.join(".git/HEAD"), "ref").expect("write");
        fs::write(root.join(".syncer/folder"), "id").expect("write");

        let rules = Rules::new(root, true, Default::default());
        let walked = walk(root, &rules, &Scope::All, Folding::default());
        let paths: Vec<_> = walked.files.keys().map(String::as_str).collect();

        assert_eq!(paths, [".gitignore", "src/deep/a.rs"]);
    }

    #[test]
    fn walks_only_the_scope() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).expect("mkdir");
        fs::write(root.join("a/b/c"), "c").expect("write");
        fs::write(root.join("x"), "x").expect("write");

        let rules = Rules::new(root, true, Default::default());
        let scope = Scope::Paths(vec!["a".into(), "gone".into()]);
        let walked = walk(root, &rules, &scope, Folding::default());

        assert_eq!(walked.files.keys().collect::<Vec<_>>(), ["a/b/c"]);
        assert!(scope.covers("a/b/c"));
        assert!(scope.covers("gone"));
        assert!(!scope.covers("ab"));
        assert!(!scope.covers("x"));
        assert!(
            within("anything/at/all", ""),
            "an unreadable root protects everything"
        );
    }
}
