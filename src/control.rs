//! The command line talking to the running service: one request per TCP
//! connection on localhost, with a token only this user can read. See
//! DESIGN.md, "Local control".

use crate::{
    folder::Settings, home::Home, ids::FolderId, net::wire, report::Report, service::Service,
    util::write_private,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::task::AbortOnDropHandle;

#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    Status,
    Invite,
    Join {
        ticket: String,
    },
    Rename {
        name: String,
    },
    Forget {
        device: String,
    },
    Add {
        path: PathBuf,
        label: Option<String>,
        use_gitignore: bool,
    },
    Accept {
        folder: String,
        path: PathBuf,
    },
    AutoAccept {
        dir: Option<PathBuf>,
    },
    Remove {
        folder: String,
        everywhere: bool,
    },
    Pause {
        folder: String,
        paused: bool,
    },
    Rescan {
        folder: String,
    },
    Reset {
        folder: String,
    },
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    Done(String),
    Report(Box<Report>),
    Failed(String),
}

#[derive(Serialize, Deserialize)]
struct Address {
    port: u16,
    token: String,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    token: String,
    command: Command,
}

/// The control listener; dropping it stops listening and, if the address
/// file is still this listener's, removes it.
pub struct Listener {
    _task: AbortOnDropHandle<()>,
    file: PathBuf,
    token: String,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let ours = std::fs::read(&self.file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Address>(&bytes).ok())
            .is_some_and(|address| address.token == self.token);
        if ours {
            let _ = std::fs::remove_file(&self.file);
        }
    }
}

/// Listens for commands until dropped. `stop` is told when a `Stop` arrives.
pub async fn listen(service: Arc<Service>, stop: Arc<tokio::sync::Notify>) -> Result<Listener> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("listening for commands")?;
    let token = hex::encode(rand::random::<[u8; 32]>());
    let address = Address {
        port: listener.local_addr()?.port(),
        token: token.clone(),
    };
    let file = service.home().control();
    write_private(&file, &serde_json::to_vec(&address)?)?;

    let listener_token = token.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let (service, token, stop) = (service.clone(), token.clone(), stop.clone());
            tokio::spawn(async move {
                if let Err(e) = answer(stream, &service, &token, &stop).await {
                    tracing::debug!("a command failed: {e:#}");
                }
            });
        }
    });
    Ok(Listener {
        _task: AbortOnDropHandle::new(task),
        file,
        token: listener_token,
    })
}

async fn answer(
    mut stream: TcpStream,
    service: &Arc<Service>,
    token: &str,
    stop: &tokio::sync::Notify,
) -> Result<()> {
    let envelope: Envelope = wire::expect(&mut stream).await?;
    if envelope.token != token {
        bail!("a command with the wrong token");
    }

    let stopping = matches!(envelope.command, Command::Stop);
    let reply = match run(service, envelope.command).await {
        Ok(reply) => reply,
        Err(e) => Reply::Failed(format!("{e:#}")),
    };
    wire::send(&mut stream, &reply).await?;

    if stopping {
        stop.notify_one();
    }
    Ok(())
}

async fn run(service: &Arc<Service>, command: Command) -> Result<Reply> {
    let folder = |query: &str| -> Result<FolderId> {
        service.cluster().find_folder(query).with_context(|| {
            format!("no folder is called {query:?} (use its id if two share a label)")
        })
    };

    Ok(match command {
        Command::Status => Reply::Report(Box::new(service.report())),
        Command::Invite => Reply::Done(service.invite().await?),
        Command::Join { ticket } => {
            let inviter = service.join(&ticket).await?;
            Reply::Done(format!(
                "Joined {inviter}'s devices. Folders they share show up in `syncer status`."
            ))
        }
        Command::Rename { name } => {
            service.rename(&name)?;
            Reply::Done(format!("This device is now called {name}."))
        }
        Command::Forget { device } => {
            let cluster = service.cluster();
            let id = cluster
                .find_device(&device)
                .with_context(|| format!("no device is called {device:?}"))?;
            service.forget_device(&id)?;
            Reply::Done(format!("{} is no longer trusted.", cluster.name_of(&id)))
        }
        Command::Add {
            path,
            label,
            use_gitignore,
        } => {
            let id = service.add_folder(&path, label, Settings { use_gitignore })?;
            Reply::Done(format!(
                "Sharing {} as {id}. Other devices can accept it now.",
                path.display()
            ))
        }
        Command::Accept {
            folder: query,
            path,
        } => {
            let id = folder(&query)?;
            service.accept_folder(&id, &path)?;
            Reply::Done(format!("Syncing {id} into {}.", path.display()))
        }
        Command::AutoAccept { dir } => {
            let dir = match dir {
                Some(dir) => Some(std::path::absolute(&dir)?),
                None => None,
            };
            let message = match &dir {
                Some(dir) => format!(
                    "Offered folders are accepted into {} from now on.",
                    dir.display()
                ),
                None => "Offered folders wait to be accepted.".to_owned(),
            };
            service.change_config(|config| config.auto_accept = dir)?;
            Reply::Done(message)
        }
        Command::Remove {
            folder: query,
            everywhere,
        } => {
            let id = folder(&query)?;
            service.remove_folder(&id, everywhere)?;
            Reply::Done(if everywhere {
                format!("{id} is no longer shared. The files stay on every device.")
            } else {
                format!("{id} no longer syncs here. Its files stay.")
            })
        }
        Command::Pause {
            folder: query,
            paused,
        } => {
            let id = folder(&query)?;
            service.pause(&id, paused)?;
            Reply::Done(format!(
                "{id} {}.",
                if paused { "is paused" } else { "syncs again" }
            ))
        }
        Command::Rescan { folder: query } => {
            let id = folder(&query)?;
            service.rescan(&id).await?;
            Reply::Done(format!("{id} scanned."))
        }
        Command::Reset { folder: query } => {
            let id = folder(&query)?;
            service.reset(&id)?;
            Reply::Done(format!(
                "{id} starts over from a fresh scan. Nothing is deleted."
            ))
        }
        Command::Stop => Reply::Done("Stopping.".to_owned()),
    })
}

/// Sends `command` to the service running for `home`.
pub async fn call(home: &Home, command: Command) -> Result<Reply> {
    let address: Address = home
        .read_json(&home.control())?
        .context("the syncer service is not running (start it with `syncer run`)")?;
    let mut stream = TcpStream::connect(("127.0.0.1", address.port))
        .await
        .context("the syncer service is not running (start it with `syncer run`)")?;
    wire::send(
        &mut stream,
        &Envelope {
            token: address.token,
            command,
        },
    )
    .await?;
    wire::expect(&mut stream).await
}
