//! The part of `.git/config` a new device takes from a peer. A config can make
//! Git run programs (`core.fsmonitor`, `core.sshCommand`, aliases, filters,
//! credential helpers, `ext::` remotes), so only keys that say where the
//! remotes are and which branches track them travel, and nothing that names
//! a program.

/// Whether `key = value` from a peer may be written to this device's config.
pub fn allowed(key: &str, value: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let Some((section, rest)) = key.split_once('.') else {
        return false;
    };
    let Some((subsection, name)) = rest.rsplit_once('.') else {
        return matches!((section, rest), ("user", "name" | "email"));
    };

    if subsection.is_empty() || value.contains('\0') || value.contains('\n') {
        return false;
    }

    match (section, name) {
        ("remote", "url" | "pushurl") => safe_url(value),
        ("remote", "fetch" | "push") => !value.trim_start().starts_with('!'),
        ("remote", "tagopt") => matches!(value, "--tags" | "--no-tags"),
        ("branch", "remote" | "pushremote" | "merge") => !value.starts_with('-'),
        ("branch", "rebase") => matches!(value, "true" | "false" | "merges" | "interactive"),
        _ => false,
    }
}

/// A URL Git fetches from without running anything but its own transports.
fn safe_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();

    if lower.starts_with('-') || lower.contains("::") {
        // `ext::<command>` runs a program; `<transport>::<address>` picks a
        // remote helper by name.
        return false;
    }

    match lower.split_once("://") {
        Some((scheme, _)) => matches!(
            scheme,
            "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git" | "file"
        ),
        // scp-like `user@host:path`, or a local path.
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::allowed;

    #[test]
    fn remotes_and_tracking_travel() {
        assert!(allowed(
            "remote.origin.url",
            "git@github.com:me/project.git"
        ));
        assert!(allowed(
            "remote.origin.url",
            "https://github.com/me/project"
        ));
        assert!(allowed(
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*"
        ));
        assert!(allowed("branch.main.remote", "origin"));
        assert!(allowed("branch.main.merge", "refs/heads/main"));
        assert!(allowed("user.email", "me@example.com"));
    }

    #[test]
    fn nothing_that_runs_a_program_travels() {
        assert!(!allowed("core.fsmonitor", "evil"));
        assert!(!allowed("core.sshcommand", "evil"));
        assert!(!allowed("core.hookspath", "/tmp"));
        assert!(!allowed("alias.st", "!evil"));
        assert!(!allowed("filter.lfs.smudge", "evil"));
        assert!(!allowed("credential.helper", "evil"));
        assert!(!allowed("remote.origin.url", "ext::sh -c evil"));
        assert!(!allowed("remote.origin.url", "fd::17"));
        assert!(!allowed("remote.origin.url", "-uevil"));
        assert!(!allowed("remote.origin.uploadpack", "evil"));
        assert!(!allowed("remote.origin.receivepack", "evil"));
        assert!(!allowed("remote.origin.vcs", "evil"));
        assert!(!allowed("branch.main.rebase", "evil"));
    }
}
