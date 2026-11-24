//! Moving objects: packs to and from peers, and fetches from remotes.

use super::*;

impl Repo {
    /// The `git pack-objects` command for everything reachable from `wants`
    /// and not from `haves`, and what to write to its standard input. Its
    /// standard output is a thin pack. Wants and haves this repository lacks
    /// are left out.
    pub fn pack_command(&self, wants: &[Oid], haves: &[Oid]) -> Result<(Command, Vec<u8>)> {
        let mut revs = String::new();

        for want in wants {
            if self.has(want)? {
                revs.push_str(want.as_str());
                revs.push('\n');
            }
        }

        for have in haves {
            if self.has(have)? {
                revs.push('^');
                revs.push_str(have.as_str());
                revs.push('\n');
            }
        }

        let mut command = self.command();
        command
            .args([
                "pack-objects",
                "--revs",
                "--thin",
                "--stdout",
                "--delta-base-offset",
                "-q",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok((command, revs.into_bytes()))
    }

    /// The `git index-pack` command that reads a thin pack on its standard
    /// input and completes it with objects already here.
    pub fn unpack_command(&self) -> Command {
        let mut command = self.command();
        command
            .args(["index-pack", "--stdin", "--fix-thin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    }

    /// The `git fetch` of every remote. It never prompts: not Git, and not
    /// SSH for a passphrase.
    pub fn fetch_all_command(&self) -> Command {
        let mut command = self.command();
        command
            .args(["fetch", "--all", "--quiet"])
            .env("SSH_ASKPASS_REQUIRE", "never")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    }

    /// Starts `pack_command`, feeding its input from a thread so the pipe
    /// can never deadlock.
    pub fn pack(&self, wants: &[Oid], haves: &[Oid]) -> Result<Child> {
        let (mut command, input) = self.pack_command(wants, haves)?;
        let mut child = command.spawn().context("running git pack-objects")?;
        let mut stdin = child
            .stdin
            .take()
            .context("git pack-objects has no input")?;
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        Ok(child)
    }

    pub fn unpack(&self) -> Result<Child> {
        self.unpack_command()
            .spawn()
            .context("running git index-pack")
    }
}

/// Waits for a git child and fails with its message when it failed.
pub fn finish(child: Child, what: &str) -> Result<()> {
    let output = child
        .wait_with_output()
        .with_context(|| format!("waiting for git {what}"))?;

    if !output.status.success() {
        bail!(
            "git {what} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

/// Drains a child's standard error on a thread, so it can never block on a
/// full pipe while its standard output is read.
pub fn drain_stderr(child: &mut Child) -> std::thread::JoinHandle<String> {
    let stderr = child.stderr.take();
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_string(&mut text);
        }
        text
    })
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn packs_carry_history_between_repositories() {
        let (_a_dir, a) = repo();
        let base = a.head_commit().expect("HEAD").expect("a commit");
        let (_b_dir, b) = repo();
        // b starts from a copy of a's first commit.
        let mut pack = a.pack(std::slice::from_ref(&base), &[]).expect("pack");
        let mut unpack = b.unpack().expect("unpack");
        let mut out = pack.stdout.take().expect("stdout");
        let mut input = unpack.stdin.take().expect("stdin");
        std::io::copy(&mut out, &mut input).expect("copied");
        drop(input);
        finish(pack, "pack-objects").expect("packed");
        finish(unpack, "index-pack").expect("unpacked");
        assert!(b.complete(&base).expect("complete"));

        // A thin pack on top of what b has.
        let tip = commit(&a, "more");
        let mut pack = a
            .pack(std::slice::from_ref(&tip), std::slice::from_ref(&base))
            .expect("pack");
        let mut unpack = b.unpack().expect("unpack");
        let mut out = pack.stdout.take().expect("stdout");
        let mut input = unpack.stdin.take().expect("stdin");
        std::io::copy(&mut out, &mut input).expect("copied");
        drop(input);
        finish(pack, "pack-objects").expect("packed");
        finish(unpack, "index-pack").expect("unpacked");
        assert!(b.complete(&tip).expect("complete"));
    }
}
