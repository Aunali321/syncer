//! Running folders: starting and stopping a worker per folder to match the
//! cluster and the local settings.

use crate::{
    folder::{Folder, Settings, live::Stopped},
    home::{AcceptNew, LocalFolder},
    ids::FolderId,
    sync::worker,
    util::lock,
};
use std::{collections::BTreeMap, sync::Weak};
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;

use super::*;

impl Service {
    /// Starts and stops folder workers to match the cluster and the local
    /// settings, and accepts offered folders when told to.
    fn reconcile(&self) {
        self.auto_accept();
        let config = lock(&self.config).clone();
        let cluster = lock(&self.cluster).clone();
        let me = self.me();
        let active = cluster.is_member(&me);

        let wanted: BTreeMap<FolderId, (LocalFolder, Settings)> = config
            .folders
            .iter()
            .filter(|(_, local)| active && !local.paused)
            .filter_map(|(id, local)| {
                Some((id.clone(), (local.clone(), cluster.folder(id)?.settings)))
            })
            .collect();

        let stopped: Vec<Running> = lock(&self.folders)
            .extract_if(.., |id, running| {
                wanted.get(id).is_none_or(|(local, settings)| {
                    local.path != running.path || *settings != running.settings
                })
            })
            .map(|(_, running)| running)
            .collect();

        for running in stopped {
            let Running { folder, _task, .. } = running;
            drop(_task);
            tracing::info!("{}: stopped", folder.id);
            tokio::spawn(async move {
                if let Err(e) = folder.save().await {
                    tracing::warn!("{}: saving: {e:#}", folder.id);
                }
            });
        }

        for (id, (local, settings)) in wanted {
            if let Some(running) = lock(&self.folders).get(&id) {
                running.peers.send_if_modified(|peers| {
                    let holders = cluster.holders(&id);
                    let moved = *peers != holders;
                    *peers = holders;
                    moved
                });
                continue;
            }

            if let Some(managed) = crate::syncthing::managing(&local.path) {
                lock(&self.failed).insert(id, Stopped::Other(managed));
                continue;
            }

            match Folder::open(
                id.clone(),
                &local.path,
                &self.home.folder_state(&id),
                *me.as_bytes(),
                settings,
            ) {
                Ok(folder) => {
                    lock(&self.failed).remove(&id);
                    let peers = watch::Sender::new(cluster.holders(&id));
                    let task = tokio::spawn(worker::run(
                        self.node.clone(),
                        folder.clone(),
                        peers.subscribe(),
                    ));
                    tracing::info!("{id}: syncing {}", local.path.display());
                    lock(&self.folders).insert(
                        id.clone(),
                        Running {
                            folder,
                            settings,
                            path: local.path.clone(),
                            peers,
                            _task: AbortOnDropHandle::new(task),
                        },
                    );
                    if !cluster.holds(&me, &id)
                        && let Err(e) =
                            self.change_cluster(|cluster| cluster.set_holding(&id, true))
                    {
                        tracing::warn!("{id}: {e:#}");
                    }
                }
                Err(e) => {
                    let stopped = Stopped::from(&e);
                    let mut failed = lock(&self.failed);
                    if failed.get(&id) != Some(&stopped) {
                        tracing::warn!("{id}: not syncing: {}", crate::util::describe(&e));
                    }
                    failed.insert(id, stopped);
                }
            }
        }
    }

    /// Accepts every offered folder into the auto-accept directory.
    fn auto_accept(&self) {
        let mode = lock(&self.config).accept_new.clone();
        if mode == AcceptNew::Ask {
            return;
        }
        let offers: Vec<(FolderId, String)> = {
            let cluster = lock(&self.cluster);
            let config = lock(&self.config);
            cluster
                .folders()
                .filter(|(id, _)| {
                    !config.folders.contains_key(*id) && !cluster.holders(id).is_empty()
                })
                .filter(|(id, _)| {
                    !cluster
                        .state()
                        .members
                        .get(*id)
                        .and_then(|m| m.get(&cluster.me()))
                        .is_some_and(|r| !r.value)
                })
                .map(|(id, folder)| (id.clone(), folder.label.clone()))
                .collect()
        };

        for (id, label) in offers {
            let path = match &mode {
                AcceptNew::Into(dir) => Some(dir.join(safe_name(&label, &id))),
                AcceptNew::SamePlace | AcceptNew::Ask => None,
            };
            match self.accept_folder(&id, path.as_deref()) {
                Ok(path) => tracing::info!("{id}: accepted into {}", path.display()),
                Err(e) => tracing::warn!("{id}: not accepted: {e:#}"),
            }
        }
    }
}

pub(super) async fn manage_folders(service: Weak<Service>) {
    let (mut cluster, mut config) = match service.upgrade() {
        Some(service) => (
            service.cluster_changes.subscribe(),
            service.config_changes.subscribe(),
        ),
        None => return,
    };

    loop {
        let Some(strong) = service.upgrade() else {
            return;
        };
        strong.reconcile();
        drop(strong);

        tokio::select! {
            _ = cluster.changed() => {}
            _ = config.changed() => {}
            () = tokio::time::sleep(FOLDER_CHECK) => {}
        }
    }
}
