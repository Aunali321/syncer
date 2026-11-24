//! Syncthing's `.stignore` patterns, read the way Syncthing reads them, to
//! tell whether a directory is left out of a Syncthing folder whole.

use globset::{GlobBuilder, GlobMatcher};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// One `.stignore` line.
struct Pattern {
    negated: bool,
    /// The pattern as paths from the folder root are matched against it:
    /// rooted, or prefixed with `**/` to match at any depth.
    rooted: Vec<String>,
    /// `rooted`, and each with `/**` for what lies under it.
    globs: Vec<GlobMatcher>,
    case_insensitive: bool,
}

/// A Syncthing folder's ignore patterns, in order: the first that matches
/// a path decides it.
#[derive(Default)]
pub struct Ignores(Vec<Pattern>);

impl Ignores {
    /// The patterns in `file` and the files it includes; none when there is
    /// no such file.
    pub fn read(file: &Path) -> Self {
        let mut ignores = Self::default();
        ignores.add_file(file, &mut HashSet::new());
        ignores
    }

    fn add_file(&mut self, file: &Path, seen: &mut HashSet<PathBuf>) {
        let Ok(text) = std::fs::read_to_string(file) else {
            return;
        };
        if !seen.insert(file.to_owned()) {
            return;
        }
        let dir = file.parent().unwrap_or(Path::new(""));

        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with("//") || line.starts_with("#escape") {
                continue;
            }
            if let Some(included) = line.strip_prefix("#include") {
                self.add_file(&dir.join(included.trim()), seen);
                continue;
            }
            if let Some(pattern) = Pattern::parse(line) {
                self.0.push(pattern);
            }
        }
    }

    /// Whether Syncthing leaves out the directory at `parts` (from the
    /// folder root) and everything under it. Sure answers only: a negation
    /// that might bring back something under it, before the pattern that
    /// leaves it out, means no.
    pub fn leave_out(&self, parts: &[String]) -> bool {
        let path = parts.join("/");

        for pattern in &self.0 {
            if pattern.matches(&path) {
                return !pattern.negated;
            }
            if pattern.negated && pattern.may_match_under(parts) {
                return false;
            }
        }
        false
    }
}

impl Pattern {
    fn parse(line: &str) -> Option<Self> {
        let (mut rest, mut negated, mut case_insensitive) = (line, false, false);
        loop {
            if let Some(after) = rest.strip_prefix('!') {
                negated = true;
                rest = after;
            } else if let Some(after) = rest.strip_prefix("(?i)") {
                case_insensitive = true;
                rest = after;
            } else if let Some(after) = rest.strip_prefix("(?d)") {
                rest = after;
            } else {
                break;
            }
        }
        // Names compare without case on macOS and Windows whatever the line says.
        let case_insensitive = case_insensitive || cfg!(any(target_os = "macos", windows));
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() {
            return None;
        }

        let rooted: Vec<String> = match rest.strip_prefix('/') {
            Some(anchored) => vec![anchored.to_owned()],
            None => match rest.strip_prefix("**/") {
                Some(bare) => vec![rest.to_owned(), bare.to_owned()],
                None => vec![rest.to_owned(), format!("**/{rest}")],
            },
        };
        let globs = rooted
            .iter()
            .flat_map(|pattern| [pattern.clone(), format!("{pattern}/**")])
            .map(|pattern| {
                GlobBuilder::new(&pattern)
                    .literal_separator(true)
                    .case_insensitive(case_insensitive)
                    .backslash_escape(cfg!(not(windows)))
                    .build()
                    .map(|glob| glob.compile_matcher())
            })
            .collect::<Result<Vec<_>, _>>()
            .inspect_err(|e| tracing::warn!(".stignore: {line:?}: {e}"))
            .ok()?;

        Some(Self {
            negated,
            rooted,
            globs,
            case_insensitive,
        })
    }

    fn matches(&self, path: &str) -> bool {
        self.globs.iter().any(|glob| glob.is_match(path))
    }

    /// Whether this pattern might match something under the directory at
    /// `parts`. Errs towards yes.
    fn may_match_under(&self, parts: &[String]) -> bool {
        self.rooted.iter().any(|pattern| {
            // Alternatives may hold a `/`: splitting by it would misread them.
            if pattern.contains('{') {
                return true;
            }
            let pattern: Vec<&str> = pattern.split('/').collect();
            reaches(&pattern, parts, self.case_insensitive)
        })
    }
}

/// Whether a pattern made of `pattern`'s parts can match `parts` or a path
/// under it.
fn reaches(pattern: &[&str], parts: &[String], case_insensitive: bool) -> bool {
    let (Some(first), Some(name)) = (pattern.first(), parts.first()) else {
        // The pattern ends at or above the directory (so its `/**` form
        // covers what is under it), or goes on below it.
        return true;
    };
    if first.contains("**") {
        return true;
    }
    let matches = GlobBuilder::new(first)
        .case_insensitive(case_insensitive)
        .backslash_escape(cfg!(not(windows)))
        .build()
        .map(|glob| glob.compile_matcher().is_match(name))
        .unwrap_or(true);
    matches && reaches(&pattern[1..], &parts[1..], case_insensitive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stignore(text: &str) -> (tempfile::TempDir, Ignores) {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join(".stignore"), text).expect("write");
        std::fs::write(dir.path().join("more.txt"), "/included\n").expect("write");
        let ignores = Ignores::read(&dir.path().join(".stignore"));
        (dir, ignores)
    }

    fn left_out(ignores: &Ignores, path: &str) -> bool {
        let parts: Vec<String> = path.split('/').map(str::to_owned).collect();
        ignores.leave_out(&parts)
    }

    #[test]
    fn stignore_patterns_leave_directories_out() {
        let (_dir, ignores) = stignore(
            "// what syncer syncs\n(?d)**/target\n/tools/syncer\nreseam/reseam/\n#include more.txt\n!keep-me\n",
        );

        assert!(left_out(&ignores, "tools/syncer"));
        assert!(
            left_out(&ignores, "tools/syncer/src"),
            "inside a left-out one"
        );
        assert!(left_out(&ignores, "reseam/reseam"));
        assert!(
            left_out(&ignores, "work/reseam/reseam"),
            "unanchored lines match at any depth"
        );
        assert!(left_out(&ignores, "a/b/target"));
        assert!(left_out(&ignores, "included"));
        assert!(!left_out(&ignores, "tools"));
        assert!(
            !left_out(&ignores, "notes"),
            "a later negation could bring files back"
        );
    }

    #[test]
    fn an_earlier_negation_that_reaches_inside_wins() {
        let (_dir, ignores) = stignore("!/x/keep\n/x\n!/y/keep\n/z\n");

        assert!(!left_out(&ignores, "x"), "Syncthing still syncs x/keep");
        assert!(left_out(&ignores, "z"), "!/y/keep cannot reach into z");
        let (_dir, unanchored) = stignore("!keep\n/z\n");
        assert!(!left_out(&unanchored, "z"));
    }
}
