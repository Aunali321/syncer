# syncer

Keep project folders in step across your own machines, peer to peer, without
breaking Git.

Byte-level sync tools copy `.git` like any other folder. Work on two machines
while they are apart and the repository ends up with conflict files inside
`.git`, half-moved branches and a broken index. Syncer syncs the working tree
as files and moves Git data as Git data:

- **Every branch and tag syncs.** Commits travel as packs; branches only
  fast-forward.
- **Diverged branches are kept.** `syncer status` shows the `git merge` that
  brings the other machine's commits in.
- **The checkout follows you.** Switch branches on one machine and the others
  switch too.
- **Nothing is lost.** Edits made on two machines at once keep both
  versions. An unmounted disk is noticed, not read as "everything deleted".

No servers or accounts. Devices connect directly over
[iroh](https://iroh.computer), find each other on the local network, and use
a relay when they can't connect directly. [DESIGN.md](DESIGN.md) explains how
it works.

## Install

You need Rust 1.91 or newer and `git` on every machine.

```sh
cargo install --git https://github.com/aunali321/syncer
```

## Quick start

On every machine, start the service (or see *Running in the background*):

```sh
syncer run
```

Pair the machines: `syncer invite` on one, then `syncer join <invite>` on
each other one. An invite works once, for 15 minutes.

Share a folder, then take it on the other machines. It goes to the same place
it has on the first one:

```sh
syncer add ~/code/project      # on the first machine
syncer accept project          # on the others
```

`syncer auto-accept` makes a machine take every shared folder that way. Then
check on things with `syncer status`.

## Commands

| Command | What it does |
|---|---|
| `syncer run [--relay URL] [--no-relays]` | Runs the service |
| `syncer status [--all] [--json]` | What needs attention; `--all` shows everything |
| `syncer invite` / `join <invite> [--merge]` | Pairs devices |
| `syncer name <name>` | Renames this device |
| `syncer forget <device>` | Stops trusting a device |
| `syncer add <path> [--label L] [--no-gitignore]` | Shares a folder |
| `syncer accept <folder> [path]` | Takes a shared folder |
| `syncer auto-accept [dir] \| --off` | Takes every shared folder |
| `syncer remove <folder> [--everywhere]` | Stops syncing a folder; files stay |
| `syncer pause` / `resume <folder>` | Pauses a folder here |
| `syncer rescan <folder>` | Scans a folder now |
| `syncer reset <folder>` | Starts a folder over, deleting nothing |
| `syncer stop` | Stops the service |

`--home <dir>` (or `SYNCER_HOME`) picks the data directory, so one machine
can run several devices.

## What syncs

Everything in the folder except:

- `.git` (it syncs as Git data), nested repositories and submodules;
- another folder inside it, which syncs on its own;
- what Git ignores, unless the checked-out commit tracks it.
  `--no-gitignore` turns this off;
- what `.syncerignore` files leave out. They use `.gitignore` syntax, and
  `!.env` brings back a file Git ignores.

The staging area, stashes, reflogs and hooks stay on each machine.

## When things need you

`syncer status` says what to do:

- **Conflict copies.** Two machines changed a file while apart. The newer
  edit keeps the name, the other is beside it as `name (conflict <hash>).ext`.
  Keep what you need and delete the copy.
- **Diverged branch.** Both machines committed while apart. Run the
  `git merge` it shows. Files in that repository wait until then.
- **Folder not found.** The disk isn't mounted or the folder was replaced.
  Nothing is touched. Once it's back, or if the folder is right, run
  `syncer reset <folder>`.
- **Files not synced.** A name another system can't store, a download that
  failed, or not enough disk space. Each says why.

## Running in the background

Linux, with systemd:

```sh
mkdir -p ~/.config/systemd/user
cp contrib/syncer.service ~/.config/systemd/user/
systemctl --user enable --now syncer
```

Add `loginctl enable-linger $USER` on a server that runs without you logged
in. On macOS, copy `contrib/com.syncer.plist` to `~/Library/LaunchAgents/`
and `launchctl load` it.

## Network

The service listens on one UDP port, kept across restarts; `--port` picks it.
iroh's public relays are slow for large files. Run your own
[`iroh-relay`](https://github.com/n0-computer/iroh/tree/main/iroh-relay) and
pass `--relay https://relay.example.com`, or use `--no-relays` to connect
only directly or on the local network.

## Moving from Syncthing

Syncer won't sync a folder Syncthing also syncs. Move one project at a time:
add it to the Syncthing folder's `.stignore` (`syncer add` says which line),
then add it to syncer.

## Development

```sh
cargo test
SYNCER_LOG=syncer=debug syncer run
```

The integration tests run real services side by side: `three_devices.rs`
walks through the main cases, `chaos_files.rs` and `chaos_git.rs` throw
random edits, crashes and Git abuse at them, and `edges.rs` covers odd names,
lost markers and the like one at a time.
`SYNCER_SEED=<n>` reruns a fuzz test with a seed it printed;
`SYNCER_TEST_KEEP=1` keeps the devices' files and logs.

## License

[MPL-2.0](LICENSE). The approach to conflict copies, `.git` handling and
folder markers draws on [Beep](https://github.com/flashmind-labs/beep)'s
project sync.
