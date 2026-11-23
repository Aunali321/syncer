# syncer

Keep project folders in step across your own machines, peer to peer, without
breaking Git.

Byte-level sync tools copy `.git` like any other folder. Work on two machines
while they are apart and the repository ends up with conflict files inside
`.git`, half-moved branches and a broken index. Syncer syncs the working tree
as files but moves Git data as Git data:

- **Every branch and tag syncs.** Commits travel as packs; branches only ever
  fast-forward.
- **Diverged branches are kept, not mangled.** Your branch stays, the other
  machine's tip is stored as `refs/syncer/conflicts/<device>/<branch>`, and
  `syncer status` tells you what to merge.
- **The checkout follows you.** Switch branches on the laptop and the desktop
  switches too, without Git touching its files: they arrive through file sync.
- **Working trees of different histories never mix.** File changes only flow
  between machines that have the same commit checked out.
- **Nothing is lost.** Concurrent edits become conflict copies
  (`notes (conflict 1a2b3c4d).txt`). A deletion never removes work the
  deleting machine had not seen. An unmounted disk is noticed instead of
  being read as "everything was deleted".

No servers, no accounts. Devices talk directly over [iroh](https://iroh.computer)
(QUIC, end-to-end encrypted, NAT traversal), find each other on the local
network with mDNS, and fall back to public relays when they cannot connect
directly.

[DESIGN.md](DESIGN.md) explains how it works.

## Install

You need Rust 1.91 or newer and `git` on every machine.

```sh
cargo install --git https://github.com/aunali321/syncer
```

## Quick start

Run the service on each machine (see *Running in the background* below to
start it at login):

```sh
syncer run
```

On the first machine, make an invite. It works once, for 15 minutes:

```sh
syncer invite
```

On each other machine, join with it:

```sh
syncer join syncer1...
```

Share a folder from any machine:

```sh
syncer add ~/code/project
```

Take it on the others, into any directory (empty, or not there yet):

```sh
syncer accept project ~/code/project
```

Or let a machine take every shared folder into one directory:

```sh
syncer auto-accept ~/code
```

Check on things:

```sh
syncer status
```

## Commands

| Command | What it does |
|---|---|
| `syncer run` | Runs the service in the foreground |
| `syncer status [--json]` | This device, its peers, its folders and anything that needs you |
| `syncer invite` | Prints a one-use invite for another device |
| `syncer join <invite>` | Joins the devices of an invite |
| `syncer name <name>` | Renames this device |
| `syncer forget <device>` | Stops trusting a device everywhere |
| `syncer add <path> [--label L] [--no-gitignore]` | Shares a directory |
| `syncer accept <folder> <path>` | Takes a shared folder into a directory here |
| `syncer auto-accept <dir> \| --off` | Takes every shared folder into `<dir>` |
| `syncer remove <folder> [--everywhere]` | Stops syncing a folder here (or everywhere); files stay |
| `syncer pause <folder>` / `resume <folder>` | Pauses a folder on this device |
| `syncer rescan <folder>` | Scans a folder now |
| `syncer reset <folder>` | Starts a folder over after its directory was replaced, deleting nothing |
| `syncer stop` | Stops the service |

Folders are named by their label or their id. `--home <dir>` (or
`SYNCER_HOME`) picks the data directory, so one machine can run several
devices.

## What syncs

Everything in the folder except:

- `.git`, which syncs as Git data, and nested repositories and submodules,
  which stay on their machine;
- what `.gitignore` excludes, in Git folders (`--no-gitignore` turns this
  off for a folder);
- what `.syncerignore` files exclude. They use `.gitignore` syntax, and a
  `!pattern` brings back something `.gitignore` leaves out:

  ```gitignore
  # Sync my local settings even though Git ignores them.
  !.env
  # But not the notes I keep for this machine.
  scratch/
  ```

From Git: every branch, every tag, which branch is checked out, and on a new
machine the remotes and upstream settings from `.git/config`. Not the staging
area, stashes, reflogs or hooks: those belong to one machine.

## When things need you

`syncer status` lists:

- **Conflict copies.** Two machines changed a file while apart. The newer
  edit keeps the name; the other is beside it as `name (conflict <hash>).ext`
  on every machine. Look at it, keep what you need, delete the copy.
- **Diverged branches.** Both machines committed on a branch while apart. Run
  the `git merge refs/syncer/conflicts/...` it shows on either machine; once
  the branch contains both, the other machine fast-forwards and the conflict
  ref goes away. File changes between the two machines wait until then.
- **A missing folder marker.** The folder's directory is empty or replaced,
  as when a disk is not mounted. Nothing is touched. Mount the disk, or run
  `syncer reset <folder>` if the directory is right.

## Running in the background

On Linux with systemd, as your user:

```sh
mkdir -p ~/.config/systemd/user
cp contrib/syncer.service ~/.config/systemd/user/
systemctl --user enable --now syncer
```

On a server that should run without you logged in, also run
`loginctl enable-linger $USER`.

On macOS, `contrib/com.syncer.plist` is a launchd agent: copy it to
`~/Library/LaunchAgents/` and run `launchctl load ~/Library/LaunchAgents/com.syncer.plist`.

## Network

The service listens on one UDP port, chosen on first start and kept after
that. Pass `--port` to choose it, for a firewall rule. `--no-relays` keeps
traffic off public relays and DNS lookup; devices then need to reach each
other directly or find each other on the local network.

## Moving from Syncthing

A folder inside a directory Syncthing shares does not start, because two
tools syncing the same files fight over every change. Pause or remove the
folder in Syncthing, then add it to syncer. Syncthing's `.stignore` patterns
are not read; copy the ones you need into `.syncerignore`.

## Development

```sh
cargo test                       # unit tests and the three-device test
SYNCER_LOG=syncer=debug syncer run
```

The integration tests run real services side by side on one machine:

- `tests/three_devices.rs` walks through pairing, branch switches, offline
  conflicts, diverged histories and an unmounted folder.
- `tests/chaos_files.rs` partitions devices and makes random edits, deletes,
  renames and file/directory swaps on each, then checks that they converge
  and that nothing written was lost; it also runs write storms, `kill -9`
  mid-transfer, odd names, symlinks and modes.
- `tests/chaos_git.rs` rewrites history, stops rebases on conflicts, detaches
  HEAD, deletes branches in use, leaves stale locks, runs `gc`, deletes
  `.git`, and storms commits and branch switches on three devices.

`SYNCER_SEED=<n>` reruns a fuzz or storm test with the seed a failure
printed; `SYNCER_TEST_KEEP=1` keeps the devices' directories and logs.

## License

[MPL-2.0](LICENSE). The approach to conflict copies, `.git` handling and
folder markers draws on [Beep](https://github.com/flashmind-labs/beep)'s
project sync.
