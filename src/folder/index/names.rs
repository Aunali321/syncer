//! Which paths can sync at all.

/// The longest path a peer may name.
const MAX_PATH: usize = 4096;

/// Code points HFS+ leaves out when it compares names, so `.g` + one of
/// them + `it` is `.git` on a Mac. The list Git itself guards against.
fn hfs_ignores(c: char) -> bool {
    matches!(c, '\u{200C}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{206A}'..='\u{206F}' | '\u{FEFF}')
}

/// Whether `part` names a `.git` directory on some file system: `.git` in
/// any case, with the trailing dots and spaces Windows drops or the code
/// points HFS+ ignores, or its short name.
pub fn is_git_name(part: &str) -> bool {
    let visible: String = part.chars().filter(|&c| !hfs_ignores(c)).collect();
    let trimmed = visible.trim_end_matches(['.', ' ']);
    trimmed.eq_ignore_ascii_case(".git") || visible.eq_ignore_ascii_case("git~1")
}

/// Whether Windows refuses `part` as a file name or reads it as something
/// else: a device name, a stream, a name ending in a dot or space.
pub fn windows_refuses(part: &str) -> bool {
    const DEVICES: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    let stem = part
        .split('.')
        .next()
        .unwrap_or(part)
        .trim_end()
        .to_ascii_lowercase();
    part.contains([':', '<', '>', '"', '|', '?', '*'])
        || part.ends_with(['.', ' '])
        || DEVICES.contains(&stem.as_str())
}

/// Why a path cannot sync on any device, or `None` when it can: it must be
/// plain names joined with `/`, none of them `.git` in any spelling, and not
/// syncer's own folder at the root. Every device holds names to this one
/// rule, so a name one device would refuse is never sent: whether a file
/// system can store a valid name is checked where it is written.
pub fn path_problem(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        return Some("empty path");
    }
    if path.len() > MAX_PATH {
        return Some("path is too long");
    }
    if path
        .split('/')
        .next()
        .is_some_and(|first| first.eq_ignore_ascii_case(crate::folder::OWN_DIR))
    {
        return Some("inside syncer's own .syncer directory");
    }
    path.split('/').find_map(|part| {
        if part.is_empty() || part == "." || part == ".." {
            Some("path has an empty, . or .. part")
        } else if part.contains('\\') {
            Some("contains a backslash")
        } else if part.chars().any(char::is_control) {
            Some("contains a newline or other control character")
        } else if is_git_name(part) {
            Some("reserved .git name")
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_from_peers_are_checked() {
        assert_eq!(path_problem("src/main.rs"), None);
        assert_eq!(path_problem("a/.gitignore"), None);
        assert!(
            windows_refuses("aux.c") && windows_refuses("a:b") && !windows_refuses("auxiliary")
        );
        for bad in [
            "",
            "/a",
            "a//b",
            "a/../b",
            "./a",
            "a/.git/config",
            ".GIT",
            "a\\b",
            ".syncer/folder",
            ".git./hooks/post-checkout",
            "a/.Git /config",
            "GIT~1/config",
            "a\u{7}b",
            "new\nline.txt",
            "tab\there",
            ".g\u{200c}it/hooks/post-checkout",
            "\u{feff}.git/config",
        ] {
            assert!(path_problem(bad).is_some(), "{bad}");
        }
        assert!(windows_refuses("aux.c") && windows_refuses("a:b") && windows_refuses("x."));
        assert!(!windows_refuses("auxiliary.rs") && !windows_refuses(".gitignore"));
    }
}
