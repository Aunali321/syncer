//! Scanning: recording what changed on disk.

use crate::{
    folder::{
        disk,
        rules::is_rule_file,
        scan::{self, Scope},
    },
    util::lock,
};
use anyhow::Result;
use std::sync::Arc;
use tokio::task::spawn_blocking;

use super::*;

impl Folder {
    /// Scans `scope`, records what changed, then looks at the repository.
    /// Returns paths that were changing while read, to look at again soon.
    pub async fn scan(self: &Arc<Self>, scope: Scope) -> Result<Vec<String>> {
        let _disk = self.disk_lock.lock().await;
        self.check()?;

        let previous_head = lock(&self.rules).tracked().head().cloned();
        let rules = self.reload_rules().await?;

        // Where names fold, a path the watcher reports may be recorded under
        // another spelling (`README` renamed to `readme`): look at both.
        let (scope, folding) = {
            let state = lock(&self.state);
            let scope = match scope {
                Scope::Paths(paths) => {
                    let aliases: Vec<String> = paths
                        .iter()
                        .flat_map(|path| state.disk.aliases(path))
                        .collect();
                    Scope::Paths(paths.into_iter().chain(aliases).collect())
                }
                Scope::All => Scope::All,
            };
            (scope, state.disk.folding())
        };
        let walked = {
            let (root, rules, scope) = (self.root.clone(), rules.clone(), scope.clone());
            spawn_blocking(move || scan::walk(&root, &rules, &scope, folding)).await?
        };
        {
            let names = &mut lock(&self.problems).names;
            names.retain(|path, _| !scope.covers(path));
            names.extend(walked.refused.clone());
        }
        let plan = lock(&self.state).plan_scan(&walked, &scope, &rules);
        let root = self.root.clone();
        let read = spawn_blocking(move || {
            plan.read
                .into_iter()
                .map(|to_read| {
                    let meta = disk::describe(&disk::join(&root, &to_read.path), &to_read.stat);
                    (to_read, meta)
                })
                .collect::<Vec<_>>()
        })
        .await?;

        let rules_changed = read.iter().any(|(to_read, _)| is_rule_file(&to_read.path))
            || plan.gone.iter().any(|gone| is_rule_file(&gone.path))
            || rules.tracked().head() != previous_head.as_ref();
        let again = {
            let mut state = lock(&self.state);
            let again = state.apply_scan(read, plan.gone, plan.excluded);
            if rules_changed {
                // Paths the old rules left out may sync now, and the reverse.
                state.pend_all();
            }
            again
        };
        // Git first: batches carry the commit the checkout is at, so it must
        // be read before the new entries are announced.
        self.observe_git_locked().await;
        self.changed();
        self.work.notify_one();
        Ok(again)
    }
}
