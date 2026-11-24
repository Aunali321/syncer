//! The command line talking to the running service: one request per TCP
//! connection on localhost, with a token only this user can read. See
//! DESIGN.md, "Local control".

use crate::{
    folder::Settings,
    home::{AcceptNew, Home},
    ids::FolderId,
    net::wire,
    report::Report,
    service::Service,
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
        merge: bool,
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
        path: Option<PathBuf>,
    },
    AutoAccept {
        mode: AcceptNew,
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
        Err(e) => Reply::Failed(crate::util::describe(&e)),
    };
    wire::send(&mut stream, &reply).await?;

    if stopping {
        stop.notify_one();
    }
    Ok(())
}

async fn run(service: &Arc<Service>, command: Command) -> Result<Reply> {
    let folder = |query: &str| -> Result<(FolderId, String)> {
        let cluster = service.cluster();
        let id = cluster
            .find_folder(query)
            .with_context(|| format!("No folder named \"{query}\"."))?;
        let label = cluster
            .folder(&id)
            .map_or_else(|| id.to_string(), |info| info.label.clone());
        Ok((id, label))
    };

    Ok(match command {
        Command::Status => Reply::Report(Box::new(crate::report::build(service))),
        Command::Invite => Reply::Done(service.invite().await?),
        Command::Join { ticket, merge } => {
            let inviter = service.join(&ticket, merge).await?;
            Reply::Done(format!("Connected to {inviter}."))
        }
        Command::Rename { name } => {
            service.rename(&name)?;
            Reply::Done(format!("Device renamed to \"{}\".", name.trim()))
        }
        Command::Forget { device } => {
            let cluster = service.cluster();
            let id = cluster
                .find_device(&device)
                .with_context(|| format!("No device named \"{device}\"."))?;
            service.forget_device(&id)?;
            Reply::Done(format!(
                "Removed {}. It can no longer sync with your devices.",
                cluster.name_of(&id)
            ))
        }
        Command::Add {
            path,
            label,
            use_gitignore,
        } => {
            let id = service.add_folder(&path, label, Settings { use_gitignore })?;
            let (_, label) = folder(id.as_str())?;
            Reply::Done(format!("Sharing {label}."))
        }
        Command::Accept {
            folder: query,
            path,
        } => {
            let (id, label) = folder(&query)?;
            let path = service.accept_folder(&id, path.as_deref())?;
            Reply::Done(format!("Syncing {label} to {}.", path.display()))
        }
        Command::AutoAccept { mode } => {
            let message = match &mode {
                AcceptNew::Ask => {
                    "Auto-accept off. New folders wait for `syncer accept`.".to_owned()
                }
                AcceptNew::SamePlace => {
                    "Auto-accept on. New folders go where they are on the sharing device."
                        .to_owned()
                }
                AcceptNew::Into(dir) => {
                    format!("Auto-accept on. New folders go into {}.", dir.display())
                }
            };
            service.change_config(|config| config.accept_new = mode)?;
            Reply::Done(message)
        }
        Command::Remove {
            folder: query,
            everywhere,
        } => {
            let (id, label) = folder(&query)?;
            service.remove_folder(&id, everywhere)?;
            Reply::Done(if everywhere {
                format!("Stopped sharing {label}. Files stay on every device.")
            } else {
                format!("Stopped syncing {label} here. Files stay.")
            })
        }
        Command::Pause {
            folder: query,
            paused,
        } => {
            let (id, label) = folder(&query)?;
            service.pause(&id, paused)?;
            Reply::Done(if paused {
                format!("Paused {label}.")
            } else {
                format!("Resumed {label}.")
            })
        }
        Command::Rescan { folder: query } => {
            let (id, label) = folder(&query)?;
            service.rescan(&id).await?;
            Reply::Done(format!("Rescanned {label}."))
        }
        Command::Reset { folder: query } => {
            let (id, label) = folder(&query)?;
            service.reset(&id)?;
            Reply::Done(format!("Reset {label}. No files are deleted."))
        }
        Command::Stop => Reply::Done("Stopping.".to_owned()),
    })
}

const NOT_RUNNING: &str = "syncer isn't running. Start it with: syncer run";

/// Sends `command` to the service running for `home`.
pub async fn call(home: &Home, command: Command) -> Result<Reply> {
    let address: Address = home.read_json(&home.control())?.context(NOT_RUNNING)?;
    let mut stream = TcpStream::connect(("127.0.0.1", address.port))
        .await
        .context(NOT_RUNNING)?;
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
