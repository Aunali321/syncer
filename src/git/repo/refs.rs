//! Reading and moving refs and HEAD.

use super::*;

impl Repo {
    pub fn head(&self) -> Result<HeadTarget> {
        let output = self.output(["symbolic-ref", "-q", "HEAD"])?;

        if output.status.success() {
            let name = String::from_utf8(output.stdout)?.trim().to_owned();
            let short = name
                .strip_prefix(HEADS)
                .with_context(|| format!("HEAD names {name}, which is not a branch"))?;
            return Ok(HeadTarget::Branch(short.to_owned()));
        }

        Ok(HeadTarget::Detached(
            self.resolve("HEAD")?.context("HEAD names no commit")?,
        ))
    }

    /// The commit HEAD is at; `None` on an unborn branch.
    pub fn head_commit(&self) -> Result<Option<Oid>> {
        self.resolve("HEAD")
    }

    /// The object `name` resolves to, or `None` when it names nothing.
    pub fn resolve(&self, name: &str) -> Result<Option<Oid>> {
        let output = self.output(["rev-parse", "-q", "--verify", &format!("{name}^{{object}}")])?;

        if !output.status.success() {
            return Ok(None);
        }

        Ok(Some(Oid::try_from(
            String::from_utf8(output.stdout)?.trim().to_owned(),
        )?))
    }

    /// Every ref under `prefix`, by full name.
    pub fn refs(&self, prefix: &str) -> Result<BTreeMap<String, Oid>> {
        let listed = self.run(["for-each-ref", "--format=%(objectname) %(refname)", prefix])?;
        let mut refs = BTreeMap::new();

        for line in listed.lines() {
            let (oid, name) = line.split_once(' ').context("a ref line without a name")?;
            refs.insert(name.to_owned(), Oid::try_from(oid.to_owned())?);
        }

        Ok(refs)
    }

    /// The paths HEAD's tree holds; none on an unborn branch. Names that are
    /// not UTF-8 are left out: they never sync.
    pub fn tracked(&self) -> Result<BTreeSet<String>> {
        if self.head_commit()?.is_none() {
            return Ok(BTreeSet::new());
        }
        let output = self.output(["ls-tree", "-r", "-z", "--name-only", "--full-tree", "HEAD"])?;
        ensure!(
            output.status.success(),
            "git ls-tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output
            .stdout
            .split(|&byte| byte == 0)
            .filter_map(|name| std::str::from_utf8(name).ok())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Whether the object `oid` is in the repository.
    pub fn has(&self, oid: &Oid) -> Result<bool> {
        Ok(self
            .output(["cat-file", "-e", oid.as_str()])?
            .status
            .success())
    }

    /// Whether `ancestor` is `descendant` or in its history. Both must be here.
    pub fn contains(&self, descendant: &Oid, ancestor: &Oid) -> Result<bool> {
        if ancestor == descendant {
            return Ok(true);
        }

        let output = self.output([
            "merge-base",
            "--is-ancestor",
            ancestor.as_str(),
            descendant.as_str(),
        ])?;

        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!(
                "git merge-base failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }

    /// Whether everything `tip` needs is here: the object, its history and
    /// their trees.
    pub fn complete(&self, tip: &Oid) -> Result<bool> {
        if !self.has(tip)? {
            return Ok(false);
        }

        Ok(self
            .output([
                "rev-list",
                "--objects",
                "--quiet",
                tip.as_str(),
                "--not",
                "--all",
            ])?
            .status
            .success())
    }

    /// Sets `name` to `new` if it is at `old` now (`None`: if it does not
    /// exist), so a ref that moved under us is never overwritten.
    pub fn update_ref(&self, name: &str, new: &Oid, old: Option<&Oid>) -> Result<()> {
        let zero = "0".repeat(new.as_str().len());
        let old = old.map_or(zero.as_str(), Oid::as_str);
        self.run(["update-ref", "-m", "syncer", name, new.as_str(), old])?;
        Ok(())
    }

    /// Deletes `name` if it is at `old` now.
    pub fn delete_ref(&self, name: &str, old: &Oid) -> Result<()> {
        self.run(["update-ref", "-d", name, old.as_str()])?;
        Ok(())
    }

    /// Points HEAD at `target` without touching the work tree, then refreshes
    /// the index from it.
    pub fn set_head(&self, target: &HeadTarget) -> Result<()> {
        match target {
            HeadTarget::Branch(branch) => {
                self.run([
                    "symbolic-ref",
                    "-m",
                    "syncer",
                    "HEAD",
                    &format!("{HEADS}{branch}"),
                ])?;
            }
            HeadTarget::Detached(oid) => {
                self.run([
                    "update-ref",
                    "--no-deref",
                    "-m",
                    "syncer",
                    "HEAD",
                    oid.as_str(),
                ])?;
            }
        }

        self.reset_index()
    }

    /// Makes the index match HEAD, leaving the work tree alone: what
    /// `git reset` does. On an unborn branch the index is emptied.
    pub fn reset_index(&self) -> Result<()> {
        if self.head_commit()?.is_some() {
            self.run(["reset", "--quiet", "--mixed"])?;
        } else {
            self.run(["read-tree", "--empty"])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn reads_head_refs_and_ancestry() {
        let (_dir, repo) = repo();
        let first = repo.head_commit().expect("HEAD").expect("a commit");
        let second = commit(&repo, "second");

        assert_eq!(
            repo.head().expect("HEAD"),
            HeadTarget::Branch("main".into())
        );
        assert_eq!(repo.refs(HEADS).expect("refs")["refs/heads/main"], second);
        assert!(repo.contains(&second, &first).expect("ancestry"));
        assert!(!repo.contains(&first, &second).expect("ancestry"));
        assert!(repo.complete(&second).expect("complete"));
        assert_eq!(repo.busy(), None);
    }

    #[test]
    fn ref_updates_never_overwrite_a_moved_ref() {
        let (_dir, repo) = repo();
        let first = repo.head_commit().expect("HEAD").expect("a commit");
        let second = commit(&repo, "second");

        assert!(
            repo.update_ref("refs/heads/main", &first, Some(&first))
                .is_err()
        );
        repo.update_ref("refs/heads/topic", &first, None)
            .expect("created");
        assert!(repo.update_ref("refs/heads/topic", &second, None).is_err());
        repo.update_ref("refs/heads/topic", &second, Some(&first))
            .expect("moved");
        repo.delete_ref("refs/heads/topic", &second)
            .expect("deleted");
        assert!(
            !repo
                .refs(HEADS)
                .expect("refs")
                .contains_key("refs/heads/topic")
        );
    }

    #[test]
    fn moving_head_leaves_the_work_tree_alone() {
        let (_dir, repo) = repo();
        let first = repo.head_commit().expect("HEAD").expect("a commit");
        commit(&repo, "second");
        git(&repo, &["branch", "old", first.as_str()]);

        repo.set_head(&HeadTarget::Branch("old".into()))
            .expect("moved");

        assert_eq!(repo.head().expect("HEAD"), HeadTarget::Branch("old".into()));
        assert_eq!(
            std::fs::read_to_string(repo.root().join("file.txt")).expect("read"),
            "second"
        );
        // The index is the old commit's, so the file reads as modified.
        assert_eq!(git(&repo, &["status", "--porcelain"]).trim(), "M file.txt");
    }
}
