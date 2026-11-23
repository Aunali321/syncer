//! The background service: the network endpoint, the cluster, and a worker
//! per folder this device holds.

use crate::{
    cluster::Cluster,
    folder::{Folder, Settings},
    home::{Home, LocalConfig, LocalFolder, hostname},
    ids::{DeviceId, FolderId, short},
    net::{
        node::{self, NetOptions, Node},
        pair::{Invites, Ticket},
        proto::{ALPN, Chunk, GetReply, Hello, PAIR_ALPN, PairReply, PairRequest, PullMessage, Request},
        wire,
    },
    report::{self, Report},
    sync::{serve, worker},
    util::lock,
};
use anyhow::{Context, Result, bail, ensure};
use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    sync::watch,
    time::timeout,
};
use tokio_util::task::AbortOnDropHandle;

/// How often members exchange cluster state when nothing changed.
const GOSSIP: Duration = Duration::from_secs(60);
/// How often folders that failed to start are tried again.
const FOLDER_CHECK: Duration = Duration::from_secs(30);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

struct Running {
    folder: Arc<Folder>,
    settings: Settings,
    path: PathBuf,
    peers: watch::Sender<Vec<DeviceId>>,
    _task: AbortOnDropHandle<()>,
}

pub struct Service {
    home: Home,
    node: Arc<Node>,
    cluster: Mutex<Cluster>,
    /// Moves whenever the cluster state changes.
    cluster_changes: watch::Sender<u64>,
    config: Mutex<LocalConfig>,
    /// Moves whenever the local settings change.
    config_changes: watch::Sender<u64>,
    folders: Mutex<BTreeMap<FolderId, Running>>,
    /// Folders that could not start, with why.
    failed: Mutex<BTreeMap<FolderId, String>>,
    invites: Invites,
    router: Mutex<Option<Router>>,
    tasks: Mutex<Vec<AbortOnDropHandle<()>>>,
}

impl Service {
    pub async fn start(home: Home, options: &NetOptions) -> Result<Arc<Self>> {
        home.create()?;
        let key = home.secret_key()?;
        let me = key.public();
        let cluster = match home.read_json::<Cluster>(&home.cluster())? {
            Some(cluster) if cluster.me() == me => cluster,
            Some(_) => bail!("{} belongs to another key", home.cluster().display()),
            None => Cluster::new(me, &hostname()),
        };
        let config = home.read_json::<LocalConfig>(&home.config())?.unwrap_or_default();
        let endpoint = node::bind(key, options).await?;
        let node = Arc::new(Node::new(endpoint.clone(), home.addrs()));

        let service = Arc::new(Self {
            home,
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
        let running: Vec<Running> = std::mem::take(&mut *lock(&self.folders)).into_values().collect();

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
        lock(&self.folders).get(id).map(|running| running.folder.clone())
    }

    pub fn failed(&self) -> BTreeMap<FolderId, String> {
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
        self.cluster_changes.send_modify(|generation| *generation += 1);
        Ok(())
    }

    /// Changes the local settings and lets the folder manager act on them.
    pub fn change_config(&self, change: impl FnOnce(&mut LocalConfig)) -> Result<()> {
        change(&mut lock(&self.config));
        self.save_config()?;
        self.config_changes.send_modify(|generation| *generation += 1);
        Ok(())
    }

    /// Merges a member's cluster state and notes where it was reached.
    fn merge_cluster(&self, from: &DeviceId, hello: &Hello) -> Result<()> {
        if hello.addr.id == *from {
            self.node.remember(hello.addr.clone());
        }

        let changed = lock(&self.cluster).merge(&hello.state);
        {
            let cluster = lock(&self.cluster);
            for addr in &hello.known {
                if cluster.is_member(&addr.id) {
                    self.node.learn(addr.clone());
                }
            }
        }
        if changed {
            self.save_cluster()?;
            self.cluster_changes.send_modify(|generation| *generation += 1);
            let removed: Vec<DeviceId> = {
                let cluster = lock(&self.cluster);
                cluster
                    .state()
                    .devices
                    .iter()
                    .filter(|(_, record)| record.value.removed)
                    .map(|(id, _)| *id)
                    .collect()
            };
            for device in removed {
                self.node.drop_peer(&device);
            }
        }
        Ok(())
    }

    fn hello(&self) -> Hello {
        Hello {
            state: lock(&self.cluster).state().clone(),
            addr: self.node.addr(),
            known: self.node.known(),
        }
    }

    /// Exchanges cluster state with `peer`.
    async fn exchange(&self, peer: DeviceId) -> Result<()> {
        let connection = self.node.connect(peer).await?;
        let (mut send, mut recv) = connection.open_bi().await?;
        wire::send(&mut send, &Request::Cluster(self.hello())).await?;
        send.finish()?;
        let theirs: Hello = wire::expect(&mut recv).await?;
        self.merge_cluster(&peer, &theirs)
    }

    // Commands from the command line.

    /// A one-use invite into this cluster.
    pub async fn invite(&self) -> Result<String> {
        // Give the endpoint a moment to learn its addresses after start.
        let _ = timeout(Duration::from_secs(5), self.node.endpoint().online()).await;
        let secret = self.invites.create();
        Ok(Ticket {
            addr: self.node.addr(),
            secret,
        }
        .encode())
    }

    /// Joins the cluster a ticket invites into.
    pub async fn join(&self, ticket: &str) -> Result<String> {
        let ticket = Ticket::decode(ticket)?;
        ensure!(ticket.addr.id != self.me(), "that invite is from this device");
        let connection = timeout(node::CONNECT_TIMEOUT, self.node.endpoint().connect(ticket.addr.clone(), PAIR_ALPN))
            .await
            .context("timed out reaching the inviting device")?
            .context("reaching the inviting device")?;
        let (mut send, mut recv) = connection.open_bi().await?;
        let request = PairRequest {
            secret: ticket.secret,
            name: self.name(),
            addr: self.node.addr(),
        };
        wire::send(&mut send, &request).await?;
        send.finish()?;

        let hello = match wire::expect::<PairReply>(&mut recv).await? {
            PairReply::Welcome(hello) => hello,
            PairReply::Refused(why) => bail!("the invite was refused: {why}"),
        };
        connection.close(0u32.into(), b"joined");
        let inviter = ticket.addr.id;
        self.node.remember(hello.addr.clone());
        lock(&self.cluster).merge(&hello.state);
        for addr in hello.known {
            self.node.learn(addr);
        }
        self.save_cluster()?;
        self.cluster_changes.send_modify(|generation| *generation += 1);
        Ok(lock(&self.cluster).name_of(&inviter))
    }

    /// Shares the directory at `path` as a new folder.
    pub fn add_folder(&self, path: &Path, label: Option<String>, settings: Settings) -> Result<FolderId> {
        let path = absolute(path)?;
        ensure!(path.is_dir(), "{} is not a directory", path.display());
        self.check_free(&path)?;
        let label = label
            .or_else(|| path.file_name().map(|name| name.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "folder".to_owned());
        let id = FolderId::random();

        self.change_cluster(|cluster| {
            cluster.set_folder(id.clone(), &label, settings, false);
            cluster.set_holding(&id, true);
        })?;
        self.change_config(|config| {
            config.folders.insert(id.clone(), LocalFolder { path, paused: false });
        })?;
        Ok(id)
    }

    /// Takes a folder another device offers, into `path`.
    pub fn accept_folder(&self, id: &FolderId, path: &Path) -> Result<()> {
        let path = absolute(path)?;
        ensure!(lock(&self.cluster).folder(id).is_some(), "no folder {id} is offered");
        ensure!(!lock(&self.config).folders.contains_key(id), "this device already holds {id}");
        if path.exists() {
            ensure!(path.is_dir(), "{} is not a directory", path.display());
        }
        self.check_free(&path)?;

        self.change_config(|config| {
            config.folders.insert(id.clone(), LocalFolder { path, paused: false });
        })?;
        self.change_cluster(|cluster| cluster.set_holding(id, true))
    }

    /// Fails when `path` overlaps a folder this device already holds.
    fn check_free(&self, path: &Path) -> Result<()> {
        for (id, folder) in &lock(&self.config).folders {
            ensure!(
                !path.starts_with(&folder.path) && !folder.path.starts_with(path),
                "{} overlaps the folder {id} at {}",
                path.display(),
                folder.path.display()
            );
        }
        if let Some(managed) = crate::syncthing::managing(path) {
            bail!("{managed}");
        }
        Ok(())
    }

    /// Stops syncing a folder here; its files stay. With `everywhere`, the
    /// folder leaves the cluster for every device.
    pub fn remove_folder(&self, id: &FolderId, everywhere: bool) -> Result<()> {
        self.change_config(|config| {
            config.folders.remove(id);
        })?;
        lock(&self.failed).remove(id);
        let label = lock(&self.cluster).folder(id).map(|folder| (folder.label.clone(), folder.settings));
        self.change_cluster(|cluster| {
            cluster.set_holding(id, false);
            if everywhere && let Some((label, settings)) = label {
                cluster.set_folder(id.clone(), &label, settings, true);
            }
        })?;
        let _ = std::fs::remove_file(self.home.folder_state(id));
        Ok(())
    }

    pub fn pause(&self, id: &FolderId, paused: bool) -> Result<()> {
        ensure!(lock(&self.config).folders.contains_key(id), "this device does not hold {id}");
        self.change_config(|config| {
            if let Some(folder) = config.folders.get_mut(id) {
                folder.paused = paused;
            }
        })
    }

    /// Starts a folder over without deleting anything: for a folder whose
    /// marker is gone after a disk was replaced or remounted.
    pub fn reset(&self, id: &FolderId) -> Result<()> {
        let path = lock(&self.config)
            .folders
            .get(id)
            .map(|folder| folder.path.clone())
            .with_context(|| format!("this device does not hold {id}"))?;
        lock(&self.folders).remove(id);
        Folder::reset(&path, id, &self.home.folder_state(id))?;
        lock(&self.failed).remove(id);
        self.config_changes.send_modify(|generation| *generation += 1);
        Ok(())
    }

    pub async fn rescan(&self, id: &FolderId) -> Result<()> {
        let folder = self.folder(id).with_context(|| format!("{id} is not running here"))?;
        folder.scan(crate::folder::scan::Scope::All).await?;
        Ok(())
    }

    pub fn forget_device(&self, device: &DeviceId) -> Result<()> {
        ensure!(*device != self.me(), "this is this device; remove it from another one");
        let name = lock(&self.cluster).name_of(device);
        self.change_cluster(|cluster| cluster.set_device(*device, &name, true))?;
        self.node.drop_peer(device);
        Ok(())
    }

    pub fn rename(&self, name: &str) -> Result<()> {
        ensure!(!name.trim().is_empty(), "a name cannot be empty");
        let me = self.me();
        self.change_cluster(|cluster| cluster.set_device(me, name.trim(), false))
    }

    pub fn report(&self) -> Report {
        report::build(self)
    }

    // Folders.

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
            .filter_map(|(id, local)| Some((id.clone(), (local.clone(), cluster.folder(id)?.settings))))
            .collect();

        let stopped: Vec<Running> = lock(&self.folders)
            .extract_if(.., |id, running| {
                wanted
                    .get(id)
                    .is_none_or(|(local, settings)| local.path != running.path || *settings != running.settings)
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
                lock(&self.failed).insert(id, managed);
                continue;
            }

            match Folder::open(id.clone(), &local.path, &self.home.folder_state(&id), *me.as_bytes(), settings) {
                Ok(folder) => {
                    lock(&self.failed).remove(&id);
                    let peers = watch::Sender::new(cluster.holders(&id));
                    let task = tokio::spawn(worker::run(self.node.clone(), folder.clone(), peers.subscribe()));
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
                        && let Err(e) = self.change_cluster(|cluster| cluster.set_holding(&id, true))
                    {
                        tracing::warn!("{id}: {e:#}");
                    }
                }
                Err(e) => {
                    let message = format!("{e:#}");
                    let mut failed = lock(&self.failed);
                    if failed.get(&id) != Some(&message) {
                        tracing::warn!("{id}: not syncing: {message}");
                    }
                    failed.insert(id, message);
                }
            }
        }
    }

    /// Accepts every offered folder into the auto-accept directory.
    fn auto_accept(&self) {
        let Some(dir) = lock(&self.config).auto_accept.clone() else {
            return;
        };
        let offers: Vec<(FolderId, String)> = {
            let cluster = lock(&self.cluster);
            let config = lock(&self.config);
            cluster
                .folders()
                .filter(|(id, _)| !config.folders.contains_key(*id) && !cluster.holders(id).is_empty())
                .filter(|(id, _)| !cluster.state().members.get(*id).and_then(|m| m.get(&cluster.me())).is_some_and(|r| !r.value))
                .map(|(id, folder)| (id.clone(), folder.label.clone()))
                .collect()
        };

        for (id, label) in offers {
            let path = dir.join(safe_name(&label, &id));
            match self.accept_folder(&id, &path) {
                Ok(()) => tracing::info!("{id}: accepted into {}", path.display()),
                Err(e) => tracing::warn!("{id}: not accepted: {e:#}"),
            }
        }
    }
}

/// A directory name for a folder label: the label without path separators
/// or characters some systems refuse, else the folder id.
fn safe_name(label: &str, id: &FolderId) -> String {
    let name: String = label
        .chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let name = name.trim().trim_matches('.');
    if name.is_empty() { id.to_string() } else { name.to_owned() }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))?;
    Ok(match dunce_canonical(&path) {
        Some(real) => real,
        None => path,
    })
}

/// The canonical form of `path` when it exists, without Windows' verbatim
/// prefix.
fn dunce_canonical(path: &Path) -> Option<PathBuf> {
    let real = path.canonicalize().ok()?;
    #[cfg(windows)]
    {
        let text = real.to_str()?;
        if let Some(rest) = text.strip_prefix(r"\\?\") && rest.as_bytes().get(1) == Some(&b':') {
            return Some(PathBuf::from(rest));
        }
    }
    Some(real)
}

async fn gossip(service: Weak<Service>) {
    let mut changes = match service.upgrade() {
        Some(service) => service.cluster_changes.subscribe(),
        None => return,
    };

    loop {
        let Some(service) = service.upgrade() else { return };
        let peers = lock(&service.cluster).others();
        let exchanges = peers.into_iter().map(|peer| {
            let service = service.clone();
            async move {
                match timeout(EXCHANGE_TIMEOUT, service.exchange(peer)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::debug!("cluster exchange with {}: {e:#}", short(&peer)),
                    Err(_) => tracing::debug!("cluster exchange with {} timed out", short(&peer)),
                }
            }
        });
        futures_join_all(exchanges).await;
        drop(service);

        changes.borrow_and_update();
        let _ = timeout(GOSSIP, changes.changed()).await;
    }
}

async fn futures_join_all<F: Future<Output = ()> + Send + 'static>(futures: impl IntoIterator<Item = F>) {
    let mut set = tokio::task::JoinSet::new();
    for future in futures {
        set.spawn(future);
    }
    while set.join_next().await.is_some() {}
}

async fn manage_folders(service: Weak<Service>) {
    let (mut cluster, mut config) = match service.upgrade() {
        Some(service) => (service.cluster_changes.subscribe(), service.config_changes.subscribe()),
        None => return,
    };

    loop {
        let Some(strong) = service.upgrade() else { return };
        strong.reconcile();
        drop(strong);

        tokio::select! {
            _ = cluster.changed() => {}
            _ = config.changed() => {}
            () = tokio::time::sleep(FOLDER_CHECK) => {}
        }
    }
}

/// Serves members on `ALPN`.
#[derive(Debug, Clone)]
struct Members(Weak<Service>);

impl ProtocolHandler for Members {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let peer = connection.remote_id();
        let Some(service) = self.0.upgrade() else {
            return Ok(());
        };

        if !lock(&service.cluster).is_member(&peer) {
            tracing::debug!("refusing {}: not a member", short(&peer));
            connection.close(2u32.into(), b"not a member");
            return Ok(());
        }

        service.node.adopt(&connection);
        drop(service);

        loop {
            let (send, recv) = match connection.accept_bi().await {
                Ok(streams) => streams,
                Err(_) => return Ok(()),
            };
            let service = self.0.clone();
            tokio::spawn(async move {
                let Some(service) = service.upgrade() else { return };
                if let Err(e) = serve_stream(service, peer, send, recv).await {
                    tracing::debug!("serving {}: {e:#}", short(&peer));
                }
            });
        }
    }
}

async fn serve_stream(
    service: Arc<Service>,
    peer: DeviceId,
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
) -> Result<()> {
    let request: Request = wire::expect(&mut recv).await?;

    // Every request but the cluster's is about a folder the peer holds too.
    let folder = match &request {
        Request::Cluster(_) => None,
        Request::Pull { folder, .. } | Request::Get { folder, .. } | Request::Pack { folder, .. } => {
            let allowed = lock(&service.cluster).holds(&peer, folder);
            match service.folder(folder).filter(|_| allowed) {
                Some(running) => Some(running),
                None => {
                    let why = format!("{folder} is not shared with this device here");
                    match &request {
                        Request::Pull { .. } => wire::send(&mut send, &PullMessage::Refused(why)).await?,
                        Request::Get { .. } => wire::send(&mut send, &GetReply::Missing).await?,
                        _ => wire::send(&mut send, &Chunk::Failed(why)).await?,
                    }
                    send.finish()?;
                    return Ok(());
                }
            }
        }
    };

    match (request, folder) {
        (Request::Cluster(hello), _) => {
            service.merge_cluster(&peer, &hello)?;
            wire::send(&mut send, &service.hello()).await?;
            send.finish()?;
            Ok(())
        }
        (Request::Pull { cursor, .. }, Some(folder)) => {
            drop(service);
            serve::pull(folder, send, cursor).await
        }
        (Request::Get { hash, path, offset, .. }, Some(folder)) => serve::get(folder, send, hash, path, offset).await,
        (Request::Pack { wants, haves, .. }, Some(folder)) => serve::pack(folder, send, wants, haves).await,
        _ => Ok(()),
    }
}

/// Redeems invites on `PAIR_ALPN`.
#[derive(Debug, Clone)]
struct Pairing(Weak<Service>);

impl ProtocolHandler for Pairing {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let joiner = connection.remote_id();
        let Some(service) = self.0.upgrade() else {
            return Ok(());
        };
        let Ok((mut send, mut recv)) = connection.accept_bi().await else {
            return Ok(());
        };

        let reply = match pair(&service, joiner, &mut recv).await {
            Ok(hello) => PairReply::Welcome(hello),
            Err(e) => {
                tracing::info!("refused a join from {}: {e:#}", short(&joiner));
                PairReply::Refused(format!("{e:#}"))
            }
        };
        if wire::send(&mut send, &reply).await.is_ok() {
            let _ = send.finish();
            let _ = send.stopped().await;
        }
        Ok(())
    }
}

async fn pair(service: &Service, joiner: DeviceId, recv: &mut iroh::endpoint::RecvStream) -> Result<Hello> {
    let request: PairRequest = wire::expect(recv).await?;
    service.invites.redeem(&request.secret)?;
    ensure!(request.addr.id == joiner, "the joining device named another address");
    let name = request.name.trim();
    let name = if name.is_empty() { short(&joiner) } else { name.to_owned() };

    service.change_cluster(|cluster| cluster.set_device(joiner, &name, false))?;
    service.node.remember(request.addr);
    tracing::info!("{} ({}) joined", name, short(&joiner));
    Ok(service.hello())
}
