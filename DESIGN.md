# Syncer design

Syncer keeps folders in step across your own machines, peer to peer. It is
built for folders that hold Git repositories, where byte-level sync tools
(Syncthing, Dropbox) corrupt `.git` as soon as two machines work while apart.

This document is the reference for how it works and why. The code follows it;
when they disagree, the code is wrong or this document is out of date.

## Goals

- Nothing is lost. A concurrent edit becomes a conflict copy, a diverged
  branch becomes a conflict ref, and a deletion never removes work the
  deleting machine had not seen.
- `.git` is never copied as files. Git data moves as Git data: refs and
  packs.
- Every branch and tag syncs, not only the checked-out one.
- No servers, accounts or cloud. Devices find each other through iroh
  (QUIC, end-to-end encrypted, NAT traversal, relays as a fallback, mDNS on
  the LAN).
- Plain to operate: one binary, a background service and a command line.

## Non-goals

- Syncing the Git index (staging area), stashes, reflogs, hooks or
  `refs/remotes/*`. These describe one machine, not the project.
- Merging diverged branches automatically. Syncer reports the divergence and
  leaves the merge to you.
- Version history of files beyond Git. Conflict copies are the only backups.

## Concepts

**Device.** A machine running the syncer service. Its identity is an iroh
secret key kept in the data directory; its id is the public key.

**Cluster.** The devices that trust each other. A device joins a cluster by
redeeming a one-use invite from any member. Membership and the folder catalog
replicate between members (see *Cluster state*).

**Folder.** A shared directory, identified by a random folder id and labelled
for humans. Each device that holds it maps the id to a local path, which may
differ between devices.

## Folder sync

### The index

Each device keeps an index per folder: for every path, the latest version it
knows and the file it names (or a tombstone for a deletion).

```
Entry { path, version: VersionVector, file: Option<FileMeta> }
FileMeta { kind: File | Symlink, hash: blake3, size, mtime_ms, executable }
```

A version vector maps replica ids to counters. A replica is one device's copy
of one folder's index. Comparing two vectors gives `Same`, `Newer`, `Older`
or `Concurrent`.

Merging an incoming entry into the index:

- `Newer` or `Same`: keep ours.
- `Older`: take theirs.
- `Concurrent`: both edits happened without seeing each other. The winner is
  chosen the same way on every device: a file beats a tombstone (an edit
  beats a delete), then the later mtime, then the larger hash. The path gets
  the winner's file and the union of both vectors. The loser, if it was a
  file with different content, becomes a new entry at a conflict path,
  `name (conflict 1a2b3c4d).ext`, named after its own hash. The copy is a
  new file nobody has edited yet, so it carries the empty version: every
  device derives exactly the same entry whatever order entries arrive in,
  and any later edit or deletion of the copy is newer than it.

Conflict copies are ordinary files from then on. Syncer never deletes one;
you do, once you have looked at it.

### What is on disk

Separately from the index, each device remembers what it last saw on disk
for each path: the file metadata, the stat data used to skip rehashing
(size, mtime, inode), and the **version of that content**.

The index is where the folder should be; the local record is where it is.
The difference is the work to do.

When a scan finds a path whose disk state changed, it hashes the file and
records an edit whose base is the version of the content that was on disk.
Using the on-disk version, not the index version, matters: if a newer file
arrived in the index but was not written yet, an edit made to the old file is
correctly concurrent with it and produces a conflict copy instead of silently
replacing it.

### Applying changes

For each path where the index differs from the disk:

1. Check that the disk still matches the local record. If the user changed
   the file since the last scan, skip it; the next scan records their edit
   and the merge decides.
2. Get the content: copy it from a local file with the same hash (renames,
   duplicates), or fetch it from a peer that announced this entry.
3. Write it to a temporary file in `.syncer/tmp` (same file system as the
   target), check its hash, fsync, check the disk record again, and rename it
   over the target. A partial download stays there and the next attempt
   resumes from its end.

Creations and updates go before deletions, so a renamed file is copied from
its old path before that path is removed. A deletion that leaves a directory
empty removes the directory. Empty directories are not synced.

On case-insensitive file systems a new path whose name differs only by case
from an existing one is refused and reported, never written over it.

### Sequence numbers and pulling

Every change to a device's index gets the next local sequence number. A peer
pulls with a cursor `(replica, seq)`: "send everything after `seq`". The
sender streams the changes, then keeps the stream open and sends new changes
as they happen.

A device that restarts after a crash may load an index older than what peers
already received. On load, the sequence number and the replica's own counter
jump forward by 2^32, so no number given out before the crash is reused. A
device whose state is lost entirely starts with a new replica id, and peers
pull from it from zero.

### Scanning and watching

A scan walks the folder with the rules below, compares stat data with the
local record, and hashes what changed. File system events trigger scans of
the paths they name, debounced. A full scan runs at start and every ten
minutes, and whenever the watcher reports an overflow.

### Rules

Paths are excluded in this order:

1. Always: `.git` at any depth, `.syncer/`, and any
   directory that holds its own `.git` (nested repositories and submodules
   stay on their machine).
2. `.syncerignore` files, in `.gitignore` syntax, at any depth. A `!pattern`
   here re-includes something `.gitignore` excludes, for example `!.env`.
3. In Git folders, `.gitignore` files and `.git/info/exclude`. Ignored build
   output is not synced. This can be turned off per folder.

An excluded path is invisible: it is never scanned, never written, and a path
becoming excluded does not delete it anywhere.

### The folder marker

`.syncer/folder` in the folder root holds the folder id. A scan that finds it
missing or naming another folder stops the folder with an error instead of
reading every file as deleted. This is what keeps an unmounted disk or a
replaced directory from wiping the other devices. `syncer reset <folder>`
starts the folder over from a fresh scan that deletes nothing.

## Git sync

A folder whose root holds a `.git` directory is a Git folder. Syncer never
writes inside `.git` directly; it runs `git`.

### What syncs

- Branches: every `refs/heads/*`.
- Tags: every `refs/tags/*`, created where missing, never moved or deleted.
- HEAD: which branch is checked out, or which commit when detached.
- A sanitized `.git/config` on first copy only (remotes and branch upstreams;
  nothing that can make Git run a program).

### Branches

Each device advertises its branch tips and its **branch tombstones**. A
tombstone records that a branch was deleted while at commit `C`.

For each branch a peer advertises, the receiving device decides:

| Local branch | Peer tip | Action |
|---|---|---|
| missing, no tombstone | `T` | create at `T` |
| missing, tombstone at `C` | `T` contained in `C` | nothing (the peer will delete) |
| missing, tombstone at `C` | `T` not in `C` | create at `T` (new work wins) |
| at `L` | `T == L` | nothing |
| at `L` | `L` is an ancestor of `T` | fast-forward to `T` |
| at `L` | `T` is an ancestor of `L` | nothing (the peer will fast-forward) |
| at `L` | diverged | keep `L`, store `T` as `refs/syncer/conflicts/<device>/<branch>` |

For a tombstone `(branch, C)` a peer advertises, a local branch at `L` is
deleted only when `L` is contained in `C`. A branch that moved on since stays
and its tip spreads back. A branch that is checked out is never deleted.

Ref updates use `git update-ref <ref> <new> <old>`, so a branch that changed
under syncer is never overwritten. A conflict ref is removed once the local
branch contains it, which is what happens after you merge it.

Objects travel before refs move: the receiver asks for a pack of the wanted
tips, the sender runs `git pack-objects --revs` excluding commits both have,
the receiver runs `git index-pack --fix-thin` and checks that the new tips are
complete before updating any ref.

### HEAD and the working tree

The working tree is shared, so all devices must agree on what is checked out.
HEAD is replicated as a last-writer-wins register stamped with a Lamport
clock and the device id. When a peer's HEAD wins, the device points HEAD at
the same branch (or commit) and refreshes the index from it. **Git never
touches the working tree**: the files of the new checkout arrive through file
sync, because they changed on the peer's disk when it checked out.

A fast-forward of the checked-out branch does the same: the ref moves, the
index is refreshed, the files arrive (or already arrived) as files.

Staged-but-uncommitted changes on a device whose HEAD is moved by a peer
become unstaged. Their content stays in the working tree.

### Keeping working trees coherent

File changes from a peer are only accepted while both devices have the same
commit checked out. Otherwise the pull pauses and retries once Git has caught
up. In the common case Git catches up in a moment by fast-forwarding. When
branches have diverged the folder waits for the user to merge, so the working
trees of two different histories never get mixed together. `syncer status`
says which branch diverged with which device and how to merge.

### Starting a new device

A Git folder accepted into an empty directory is created with `git init`, the
sanitized config, every branch and tag, and the peer's HEAD. The working tree
then arrives through file sync.

## Devices and trust

### Pairing

`syncer invite` asks the service for a one-use invite valid for 15 minutes.
The ticket carries the inviter's address and a 32-byte secret. `syncer join
<ticket>` connects to the inviter on the pairing protocol and presents the
secret and its name. The inviter adds the device to the cluster state and
answers with the whole state, so the new device trusts every member at once.
The other members learn about it as the state spreads.

### Cluster state

A small replicated map, last writer wins by `(lamport, device)`:

- `device/<id>`: name, or removed
- `folder/<id>`: label, settings, or removed
- `member/<folder>/<device>`: whether that device holds the folder, written
  only by that device

Members exchange the whole map whenever they connect and whenever it changes.
Connections from devices that are not members, or were removed, are refused.

### Folder offers

A folder in the catalog that this device does not hold is an offer. `syncer
accept` picks a local path for it. A device can also accept automatically
into a directory of its choice.

## Wire protocol

Two ALPNs:

- `syncer/pair/1`: invite redemption, open to anyone holding a valid secret.
- `syncer/1`: everything between members. Each bi-stream starts with a
  request: `Cluster`, `Pull`, `Get` (file content, resumable by offset),
  `Pack` (Git objects).

Messages are length-prefixed MessagePack. File content and packs travel as a
sequence of chunk messages. The receiver verifies every file against its
blake3 hash before it is placed.

## Syncthing

A folder inside a directory that Syncthing shares (and has not paused) is not
started, and says why. Two tools syncing the same files fight over every
change. Pause the folder in Syncthing, or remove it there, and syncer takes
over at the next check.

## Local control

The service listens on a Unix socket (a named pipe on Windows) in its data
directory. The command line sends one JSON request per connection and prints
the answer.

## Known limitations

- Symlinks sync on Unix only. On Windows they are skipped.
- File permissions sync only as the executable bit.
- No delta transfer: a changed file is sent whole.
- With three or more devices, an old version of a file that reaches a device
  before the newer one that replaced it can still lose a concurrent edit and
  leave a conflict copy of the old version. Nothing is lost; there is one
  copy too many.
