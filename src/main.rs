use anyhow::Result;
use clap::{Parser, Subcommand};
use std::{path::PathBuf, process::ExitCode, sync::Arc};
use syncer::{
    control::{self, Command, Reply},
    home::{AcceptNew, Home},
    net::node::{NetOptions, Relays},
    service::Service,
};

/// Peer-to-peer folder sync between your own machines that understands Git.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Where syncer keeps its key, settings and state [env: SYNCER_HOME]
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the service in the foreground
    Run {
        /// UDP port to listen on; by default one is picked
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Do not use relays or DNS lookup: only direct addresses and the
        /// local network
        #[arg(long)]
        no_relays: bool,
        /// Use this relay (an `iroh-relay` you run) instead of the public
        /// ones; give it more than once for several
        #[arg(long = "relay", value_name = "URL", conflicts_with = "no_relays")]
        relays: Vec<iroh::RelayUrl>,
        /// Do not look for devices on the local network
        #[arg(long)]
        no_mdns: bool,
    },
    /// Show what needs attention
    Status {
        /// Show every folder and device
        #[arg(long)]
        all: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Print a one-use invite for another device (valid 15 minutes)
    Invite,
    /// Join the devices of an invite
    Join {
        ticket: String,
        /// Bring the devices this one already trusts along: the inviting
        /// device's devices trust them too
        #[arg(long)]
        merge: bool,
    },
    /// Rename this device
    Name { name: String },
    /// Stop trusting a device, by name or id
    Forget { device: String },
    /// Share a directory with your other devices
    Add {
        path: PathBuf,
        /// The name other devices see; the directory's name by default
        #[arg(long)]
        label: Option<String>,
        /// Sync files that .gitignore excludes too
        #[arg(long)]
        no_gitignore: bool,
    },
    /// Take a folder another device shares; by default to the same place
    /// as on that device
    Accept {
        folder: String,
        path: Option<PathBuf>,
    },
    /// Accept new folders without asking: where they are on the sharing
    /// device, or into a directory
    AutoAccept {
        dir: Option<PathBuf>,
        #[arg(long, conflicts_with = "dir")]
        off: bool,
    },
    /// Stop syncing a folder here; its files stay
    Remove {
        folder: String,
        /// Stop sharing it on every device
        #[arg(long)]
        everywhere: bool,
    },
    /// Pause a folder here
    Pause { folder: String },
    /// Resume a paused folder
    Resume { folder: String },
    /// Scan a folder now
    Rescan { folder: String },
    /// Start a folder over after its directory was replaced or remounted,
    /// or its saved state was damaged, without deleting anything
    Reset { folder: String },
    /// Stop the running service
    Stop,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");

    match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {}", syncer::util::describe(&e));
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let home = Home::resolve(cli.home)?;

    let command = match cli.command {
        Commands::Run {
            port,
            no_relays,
            relays,
            no_mdns,
        } => {
            let relays = match (no_relays, relays.is_empty()) {
                (true, _) => Relays::Off,
                (false, true) => Relays::Public,
                (false, false) => Relays::Own(relays),
            };
            serve(
                home,
                NetOptions {
                    port,
                    relays,
                    mdns: !no_mdns,
                },
            )
            .await?;
            return Ok(ExitCode::SUCCESS);
        }
        Commands::Status { all, json } => {
            return match control::call(&home, Command::Status).await? {
                Reply::Report(report) if json => {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                    Ok(ExitCode::SUCCESS)
                }
                Reply::Report(report) if all => {
                    print!("{}", syncer::report::Full(&report));
                    Ok(ExitCode::SUCCESS)
                }
                Reply::Report(report) => {
                    print!("{report}");
                    Ok(ExitCode::SUCCESS)
                }
                other => Ok(print_reply(other)),
            };
        }
        Commands::Invite => Command::Invite,
        Commands::Join { ticket, merge } => Command::Join { ticket, merge },
        Commands::Name { name } => Command::Rename { name },
        Commands::Forget { device } => Command::Forget { device },
        Commands::Add {
            path,
            label,
            no_gitignore,
        } => Command::Add {
            path: std::path::absolute(path)?,
            label,
            use_gitignore: !no_gitignore,
        },
        Commands::Accept { folder, path } => Command::Accept {
            folder,
            path: path.map(std::path::absolute).transpose()?,
        },
        Commands::AutoAccept { dir, off } => Command::AutoAccept {
            mode: match (off, dir) {
                (true, _) => AcceptNew::Ask,
                (false, Some(dir)) => AcceptNew::Into(std::path::absolute(dir)?),
                (false, None) => AcceptNew::SamePlace,
            },
        },
        Commands::Remove { folder, everywhere } => Command::Remove { folder, everywhere },
        Commands::Pause { folder } => Command::Pause {
            folder,
            paused: true,
        },
        Commands::Resume { folder } => Command::Pause {
            folder,
            paused: false,
        },
        Commands::Rescan { folder } => Command::Rescan { folder },
        Commands::Reset { folder } => Command::Reset { folder },
        Commands::Stop => Command::Stop,
    };

    Ok(print_reply(control::call(&home, command).await?))
}

fn print_reply(reply: Reply) -> ExitCode {
    match reply {
        Reply::Done(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Reply::Report(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Reply::Failed(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(home: Home, options: NetOptions) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SYNCER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("syncer=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let service = Service::start(home, &options).await?;
    let stop = Arc::new(tokio::sync::Notify::new());
    let control = control::listen(service.clone(), stop.clone()).await?;
    println!("syncer is running as {} ({})", service.name(), service.me());

    tokio::select! {
        () = stop.notified() => {}
        _ = tokio::signal::ctrl_c() => {}
        () = terminated() => {}
    }

    tracing::info!("stopping");
    drop(control);
    service.shutdown().await;
    Ok(())
}

#[cfg(unix)]
async fn terminated() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut signal) => {
            signal.recv().await;
        }
        Err(_) => std::future::pending().await,
    }
}

#[cfg(not(unix))]
async fn terminated() {
    std::future::pending().await
}
