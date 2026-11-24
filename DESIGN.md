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

Each apply round works out, under the state lock, what every pending path
needs, then carries it out without the lock:

1. **Stage.** The content every placement needs is put in `.syncer/tmp`
   (same file system as the folder) before any file changes: content
   already downloaded is there, content another file here holds is copied
   from it and checked against its hash. So a conflict copy can never lose
   the content it is made from to the overwrite of the original, whatever
   order the two are worked on, and a renamed file is copied, not fetched.
2. **Place.** For each path, check that the disk still matches the local
   record. If the user changed the file since the last scan, skip it; the
   next scan records their edit and the merge decides. Otherwise give the
   staged content its mode and mtime, fsync it, and rename it over the path.
3. **Remove.** Deletions go last. A deletion that leaves a directory empty
   removes the directory. Empty directories are not synced.
4. **Settle.** A path leaves the pending list only if its index entry is
   still the one the work was planned for. An entry that changed while the
   round ran keeps the path pending for the next round.

Content no file here has is fetched from a peer that holds the folder, into
a partial file in `.syncer/tmp` that the next attempt resumes. It counts only
once its blake3 hash matches. Content waiting there is served to peers too.
A path whose content is being fetched leaves the pending list for a list of
paths waiting for content, by hash, and comes back when that content
arrives, when a file here comes to hold it, or when its entry changes. So an
apply round costs what changed since the last one, not every file still
downloading; the retry timer asks again for whatever still waits.
A download starts only when the free space covers what it still needs plus
what the downloads already running may still write: downloads that share out
the last of the space would each stop short, none finishing. A path waiting
for space, or whose download failed, says so in `syncer status`.

Runs of zeros are skipped over rather than written, in downloads and local
copies alike, so a sparse file (a disk image, a database) stays sparse.

### Names

One rule decides which paths can sync, on every device: plain names joined
with `/`, none of them `.` or `..`, none holding a backslash (Windows reads it
as a separator) or a control character, none a spelling of `.git` (in any
case, with the trailing dots and spaces Windows drops, with the code points
HFS+ ignores, or as the short name `GIT~1`), and not `.syncer` at the root.
A scan leaves out a name that breaks it, or one that is not UTF-8, and lists
it in `syncer status`; it is never sent. A device still checks what it
receives against the rule, for a peer that does not keep to it.

Whether a file system can store a valid name is a separate question, asked
where the name is written. Each folder probes its file system when it opens:
whether it treats names that differ by case as one (macOS, Windows), drops
dots and spaces at the end of a name (FAT, and Windows' own calls), or treats
a name's Unicode forms as one (macOS). A new path the file system would
store under another name, or take for a path already there, is refused and
reported, never written over it. On Windows, names Windows does not allow
(`aux`, `a:b`) are refused the same way.

### A file and a directory at one path

One device can replace the file `x` with a directory `x/` while another
edits `x`. Both survive the merge, but no file system holds both. Whichever
device notices first settles it: the directory wins and the file moves to a
conflict copy beside it, recorded as an ordinary edit (the copy, then a
deletion of `x`). Every device that notices records the same edits, so the
indexes converge.

A file is also placed where an empty directory, or a tree of nothing but
empty directories, stands: those are left behind when a directory's files
move away, and empty directories do not sync. A directory with any file in
it is never removed to make room.

### Content that exists nowhere

A device can make a conflict copy of a version that its author overwrote
before anyone fetched it. No device can ever fill that copy. When every
device that holds the folder is reachable and says it does not have the
content, twice at least half a minute apart, the copy is deleted like any
other file. This only applies to conflict copies; a file the user named waits
for its content however long it takes, and one unreachable device is enough
to wait.

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
local record (size, mtime, inode and, on Unix, the inode change time, which
every write moves even when the writer puts the mtime back), and hashes what
changed. A directory that cannot be listed
protects everything under it, the root included: nothing in it counts as
deleted. A file that cannot be read is tried again on a slow timer. File system events trigger scans of
the paths they name, debounced. A full scan runs at start and every ten
minutes, and whenever the watcher reports an overflow.

### Rules

Paths are excluded in this order:

1. Always: `.git` at any depth, `.syncer/`, any directory that holds its own
   `.git` (nested repositories and submodules stay on their machine), and any
   directory that is another folder's root (it holds `.syncer/folder`).
2. `.syncerignore` files, in `.gitignore` syntax, at any depth. A `!pattern`
   here re-includes something `.gitignore` excludes, for example `!.env`.
3. In Git folders, Git's own ignore files, as Git reads them: `.gitignore`
   files, then `.git/info/exclude`, then the user's global excludes file
   (`core.excludesFile`, by default `~/.config/git/ignore`). Ignored build
   output is not synced. This can be turned off per folder.

Git's ignore files never leave out a path the checked-out commit tracks, as
`git status` still shows a tracked file that a pattern matches; nor a
directory holding one, though untracked files in it stay out. File changes
only flow between devices whose checkouts are at the same commit, so both
sides judge a path by the same tracked set. A file added with `git add -f`
syncs once it is committed.

`.git/info/exclude` and the global excludes file are each device's own: a
path one device excludes that way is simply not synced from or to it.

An excluded path is invisible: it is never scanned, never written, and a path
becoming excluded does not delete it anywhere.

### The folder marker

`.syncer/folder` in the folder root holds the folder id. A scan that finds it
missing or naming another folder stops the folder with an error instead of
reading every file as deleted. This is what keeps an unmounted disk or a
replaced directory from wiping the other devices. A folder whose state file
exists has synced here before, however few files it held: its directory is
never made anew. `syncer reset <folder>` starts the folder over from a fresh
scan that deletes nothing. A damaged state file starts the folder over by
itself, from the disk alone: everything there is recorded as new, so it
meets the other devices' entries as concurrent edits, a conflict copy where
they differ and never a deletion.

### Folders inside folders

A folder can sit inside another, as a repository's folder inside a folder of
notes and repositories. The outer folder leaves the inner one out (rule 1),
as it does a nested repository. A directory of a Git checkout cannot be its
own folder, though: the outer folder would leave out files its commits
track.

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
| at `L` | diverged | keep `L`, store `T` as `refs/syncer/<device>/<branch>` |

For a tombstone `(branch, C)` a peer advertises, a local branch at `L` is
deleted only when `L` is contained in `C`. A branch that moved on since stays
and its tip spreads back. A branch that is checked out is never deleted.

A branch that the peer has checked out, when the peer's HEAD is the newer
one (so this device's checkout is about to follow it), is in use: it is
created even over a tombstone, as an edit beats a delete for files.

Ref updates use `git update-ref <ref> <new> <old>`, so a branch that changed
under syncer is never overwritten. A conflict ref is removed once the local
branch contains it (you merged it), or once the peer no longer diverges at
that tip (it merged, reset to this device's tip, or deleted the branch).

A repository whose branches all vanish at once was deleted and made again,
or replaced; Git cannot delete the branch it has checked out, so a user
deleting branches never does that. It is not read as deletions, which would
spread and delete every branch everywhere: Git sync starts over for it.

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

HEAD only moves to a branch that exists here (a peer's newer HEAD on a branch
that has not arrived waits for it), or to any branch while this repository
has no commits at all, as a new one does.

A fast-forward of the checked-out branch does the same: the ref moves, the
index is refreshed, the files arrive (or already arrived) as files.

Staged-but-uncommitted changes on a device whose HEAD is moved by a peer
become unstaged. Their content stays in the working tree.

### Keeping working trees coherent

File changes from a peer are only accepted while both devices have the same
commit checked out. Otherwise the pull holds back: it skips the peer's file
batches without moving its cursor, keeps following the peer's Git state, and
tries Git alignment again every few seconds (a repository busy with another
Git command is skipped, not waited on). As soon as the two checkouts match,
it pulls again from its cursor.

Git is read before a scan's new entries are announced, so every batch
carries the commit its changes were made on.

In the common case Git catches up in a moment by fast-forwarding. When
branches have diverged the folder waits for the user to merge, so the working
trees of two different histories never get mixed together. `syncer status`
says which branch diverged with which device and how to merge.

A folder whose root `.git` is a file (a linked worktree or a submodule) is
not synced; sync the main checkout instead.

### Starting a new device

A Git folder accepted into an empty directory is created with `git init`, the
sanitized config, every branch and tag, and the peer's HEAD. The working tree
then arrives through file sync. The same happens when `.git` is deleted on a
device: the deletion is not mirrored, the repository comes back from a peer.

What a device last fetched from a remote (`refs/remotes/*`) is its own and
does not sync, so a new repository starts without it, and `git status`
could not compare a branch with its upstream. Once it is made, the device
runs `git fetch --all` once in the background. It never prompts, and the
remotes it reaches are only those the sanitized config lets through. When
it fails (offline, no credentials), a fetch by hand does the same.

## Devices and trust

### Pairing

`syncer invite` asks the service for a one-use invite valid for 15 minutes.
The ticket carries the inviter's address and a 32-byte secret. `syncer join
<ticket>` connects to the inviter on the pairing protocol and presents the
secret and its name. The inviter adds the device to the cluster state and
answers with the whole state, so the new device trusts every member at once.
The other members learn about it as the state spreads.

The joining device's own state spreads the same way: if it already trusts
other devices, the inviter's devices come to trust them too. `syncer join`
refuses that unless given `--merge`, so two sets of devices only become one
on purpose.

### Cluster state

A small replicated map, last writer wins by `(lamport, device)`:

- `device/<id>`: name, or removed
- `folder/<id>`: label, settings, or removed
- `member/<folder>/<device>`: whether that device holds the folder, written
  only by that device

Members exchange the whole map whenever they connect and whenever it changes;
an exchange that fails is retried within seconds. Connections from devices
that are not members are refused, and a device removed from the cluster
loses every connection and pull it has open at once.

### Folder offers

A folder in the catalog that this device does not hold is an offer. The
device that shares a folder records where it keeps it, from its home
directory, and `syncer accept` puts the folder in the same place by default,
or wherever it is told. A device can also accept every offer by itself, the
same way or into one directory. A place from a peer is only ever a plain
path under this device's home directory.

Accepting records that this device holds the folder before the folder
starts. A device asked for a folder by a member it has not yet heard holds
it exchanges cluster state with that member first, and only refuses if the
member still does not hold it: a fresh accept starts syncing at once rather
than waiting for gossip.

A folder reports "up to date" only once this device has read the whole index
of at least one other device holding it, since it started; until then it is
waiting to hear from them, and the status says when each was last heard
from. Every device keeps that time for each peer across restarts.

## Wire protocol

Two ALPNs:

- `syncer/pair/1`: invite redemption, open to anyone holding a valid secret.
- `syncer/1`: everything between members. Each bi-stream starts with a
  request: `Cluster`, `Pull`, `Get` (file content, resumable by offset),
  `Pack` (Git objects).

Paths from peers are checked against the naming rule (see "Names") before
anything is merged.

Messages are length-prefixed MessagePack. File content and packs travel as a
sequence of chunk messages. The receiver verifies every file against its
blake3 hash before it is placed.

### Reaching devices

Devices connect directly where they can: addresses remembered from the last
connection, mDNS on the local network, then hole punching. Where they
cannot, traffic goes through a relay: iroh's public relays by default, which
limit how fast one device may send, or relays you run yourself (`syncer run
--relay <url>`, an `iroh-relay` server), or none (`--no-relays`, which also
skips iroh's DNS address lookup). `syncer status` says which devices are
connected through a relay.

## Syncthing

A folder that overlaps a folder Syncthing shares (and has not paused) is not
started, and says why. Two tools syncing the same files fight over every
change. Pause the folder in Syncthing, or remove it there, and syncer takes
over at the next check.

A folder inside a Syncthing folder may start when that folder's `.stignore`
leaves out the whole directory, so one repository at a time can move from
Syncthing to syncer. The patterns are read as Syncthing reads them: the first
that matches decides, a pattern without a leading `/` matches at any depth,
a pattern also covers what is under what it names, `(?i)`, `(?d)` and
`#include` are understood, and names compare without case on macOS and
Windows. The directory counts as left out only when a pattern leaves it out
and no `!` pattern before that one could bring back anything under it.

## Local control

The service listens on a TCP port on localhost and writes the port and a
random token to `control.json` in its data directory, readable only by its
user. The command line sends one request per connection, with the token, and
prints the answer.

One service runs per data directory: it holds a lock on `lock` there, which
the system lets go when the process ends however it ends. A second `syncer
run` with the same data directory refuses to start. Two would share one
device key and overwrite each other's state.

## Known limitations

- Symlinks sync on Unix only. On Windows they are skipped.
- File permissions sync only as the executable bit.
- No delta transfer: a changed file is sent whole.
- With three or more devices, an old version of a file that reaches a device
  before the newer one that replaced it can still lose a concurrent edit and
  leave a conflict copy of the old version. Nothing is lost; there is one
  copy too many.
