//! The background service: the network endpoint, the cluster, and a worker
//! per folder this device holds.

mod folders;
mod membership;
mod protocols;
mod running;

use folders::safe_name;
use membership::gossip;
use protocols::{Members, Pairing};
use running::manage_folders;

use crate::{
    cluster::Cluster,
    folder::{Folder, Settings, live::Stopped},
    home::{Home, LocalConfig, hostname},
    ids::{DeviceId, FolderId},
    net::{
        node::{self, NetOptions, Node},
        pair::Invites,
        proto::{ALPN, PAIR_ALPN},
    },
    util::lock,
};
use anyhow::{Result, bail};
use iroh::protocol::Router;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;

/// How often folders that failed to start are tried again.
const FOLDER_CHECK: Duration = Duration::from_secs(30);
struct Running {
    folder: Arc<Folder>,
    settings: Settings,
    path: PathBuf,
    peers: watch::Sender<Vec<DeviceId>>,
    _task: AbortOnDropHandle<()>,
}

pub struct Service {
    home: Home,
    _claim: std::fs::File,
    node: Arc<Node>,
    cluster: Mutex<Cluster>,
    /// Moves whenever the cluster state changes.
    cluster_changes: watch::Sender<u64>,
    config: Mutex<LocalConfig>,
    /// Moves whenever the local settings change.
    config_changes: watch::Sender<u64>,
    folders: Mutex<BTreeMap<FolderId, Running>>,
    /// Folders that could not start, with why.
    failed: Mutex<BTreeMap<FolderId, Stopped>>,
    invites: Invites,
    router: Mutex<Option<Router>>,
    tasks: Mutex<Vec<AbortOnDropHandle<()>>>,
}

impl Service {
    pub async fn start(home: Home, options: &NetOptions) -> Result<Arc<Self>> {
        home.create()?;
        let claim = home.claim()?;
        let key = home.secret_key()?;
        let me = key.public();
        let cluster = match home.read_json::<Cluster>(&home.cluster())? {
            Some(cluster) if cluster.me() == me => cluster,
            Some(_) => bail!("{} belongs to another key", home.cluster().display()),
            None => Cluster::new(me, &hostname()),
        };
        let config = home
            .read_json::<LocalConfig>(&home.config())?
            .unwrap_or_default();
        let endpoint = node::bind(key, options, &home.port()).await?;
        let node = Arc::new(Node::new(endpoint.clone(), home.addrs(), home.seen()));

        let service = Arc::new(Self {
            home,
            _claim: claim,
            node,
            cluster: Mutex::new(cluster),
            cluster_changes: watch::Sender::new(0),
            config: Mutex::new(config),
            config_changes: watch::Sender::new(0),
            folders: Mutex::default(),
            failed: Mutex::default(),
            invites: Invites::default(),
            router: Mutex::default(),
            tasks: Mutex::default(),
        });
        service.save_cluster()?;

        let router = Router::builder(endpoint)
            .accept(ALPN, Members(Arc::downgrade(&service)))
            .accept(PAIR_ALPN, Pairing(Arc::downgrade(&service)))
            .spawn();
        *lock(&service.router) = Some(router);

        let tasks = vec![
            AbortOnDropHandle::new(tokio::spawn(gossip(Arc::downgrade(&service)))),
            AbortOnDropHandle::new(tokio::spawn(manage_folders(Arc::downgrade(&service)))),
        ];
        *lock(&service.tasks) = tasks;

        tracing::info!("device {} ({}) started", me, service.name());
        Ok(service)
    }

    /// Stops every folder, saving its state, and closes the endpoint.
    pub async fn shutdown(&self) {
        lock(&self.tasks).clear();
        let running: Vec<Running> = std::mem::take(&mut *lock(&self.folders))
            .into_values()
            .collect();

        for running in running {
            let Running { folder, _task, .. } = running;
            drop(_task);
            if let Err(e) = folder.save().await {
                tracing::warn!("{}: saving: {e:#}", folder.id);
            }
        }

        let router = lock(&self.router).take();
        if let Some(router) = router {
            let _ = router.shutdown().await;
        }
    }

    pub fn home(&self) -> &Home {
        &self.home
    }

    pub fn node(&self) -> &Arc<Node> {
        &self.node
    }

    pub fn me(&self) -> DeviceId {
        self.node.id()
    }

    pub fn name(&self) -> String {
        lock(&self.cluster).name_of(&self.me())
    }

    pub fn cluster(&self) -> Cluster {
        lock(&self.cluster).clone()
    }

    pub fn config(&self) -> LocalConfig {
        lock(&self.config).clone()
    }

    pub fn folder(&self, id: &FolderId) -> Option<Arc<Folder>> {
        lock(&self.folders)
            .get(id)
            .map(|running| running.folder.clone())
    }

    pub fn failed(&self) -> BTreeMap<FolderId, Stopped> {
        lock(&self.failed).clone()
    }

    fn save_cluster(&self) -> Result<()> {
        let cluster = lock(&self.cluster).clone();
        self.home.write_json(&self.home.cluster(), &cluster)
    }

    fn save_config(&self) -> Result<()> {
        let config = lock(&self.config).clone();
        self.home.write_json(&self.home.config(), &config)
    }

    /// Changes the cluster here and tells the others.
    pub fn change_cluster(&self, change: impl FnOnce(&mut Cluster)) -> Result<()> {
        change(&mut lock(&self.cluster));
        self.save_cluster()?;
        self.cluster_changes
            .send_modify(|generation| *generation += 1);
        Ok(())
    }

    /// Changes the local settings and lets the folder manager act on them.
    pub fn change_config(&self, change: impl FnOnce(&mut LocalConfig)) -> Result<()> {
        change(&mut lock(&self.config));
        self.save_config()?;
        self.config_changes
            .send_modify(|generation| *generation += 1);
        Ok(())
    }
}
