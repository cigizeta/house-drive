# Phase 1: the core and the command-line tool

Implementation plan, drafted 2026-10-18 for review. Implements design.md's phase 1: everything but
the tray icon and the Explorer integration, which build on it later. Nothing implemented yet.

## Layout

A Cargo workspace of two crates, so later front ends (the tray, Explorer) reuse the core:

- `crates/house-drive-core`: a library, all the logic, no printing and no process exits.
- `crates/house-drive`: the binary, `house-drive.exe`, argument parsing and output only.

Dependencies kept few: `windows` (Credential Manager, the network ID, connecting to the share),
`blake3`, `serde` with `toml` (configuration), `rusqlite` (local databases only, never over SMB).

## Configuration

One file per machine, `%LOCALAPPDATA%\house-drive\config.toml`, never in the repository; the
repository holds `examples/config.toml` with placeholders:

```toml
machine_id = "machine-a"          # unique per machine, also its journal folder's name
data_root = 'C:\example\data'     # the tree this machine caches
store_root = '\\store-machine\house-drive'   # the share; on the store machine, its local path
store_id = "00000000-0000-0000-0000-000000000000"   # written by init-store, checked every run
home_network_id = "{00000000-0000-0000-0000-000000000000}"   # Windows' network ID, see join
is_store = false                  # true on the store machine only
sync_minutes = 10
cache_limit_gb = 256
evict_to_percent = 90
trash_days = 30
settle_seconds = 60               # files modified more recently wait for a later run
batch_mb = 1024                   # a push is split into batches of about this size
```

Credentials are not here: the share's account and password live in Windows Credential Manager under
one target name, `house-drive`.

## The store's layout

```
<store_root>/
  store-id.txt                  written once by init-store
  files/<path>                  every file's current content
  trash/<date>/<path>           deleted and replaced versions, emptied after trash_days
  incoming/<machine_id>/<batch>/<path>   pushed files, created by their machine, never edited
  journal/<machine_id>/<batch>.tsv       one line per operation of a batch, created once
  catalog.tsv                   the catalog's snapshot for the other machines, replaced atomically
```

A pushing machine only ever **creates** files, in its own `incoming/` and `journal/` folders, as the
permissions require (design.md, security point 5). Everything else, moving files into `files/`,
trashing, deleting, writing the catalog, is the store machine's, on its local disk.

A journal line: `operation<TAB>path<TAB>size<TAB>blake3<TAB>modified<TAB>base`, the operation `put`
or `delete`, `base` the hash of the store's version the machine started from (`-` for a new file).
A batch's journal is written after its files, and ends with an `end<TAB><line count>` line; the
store machine never merges a batch without a complete journal. (Writing under a temporary name and
renaming is not possible: pushing machines may only create.)

**Interrupted pushes (decided 2026-10-18, step 3):** a pushing machine cannot finish a cut-short
batch, since it cannot rewrite a half-copied file. Instead, batch names are increasing numbers
(nanoseconds since the Unix epoch, always above the machine's newest batch), and a machine pushes
its batches one after another, so a batch with an incomplete journal older than its machine's
newest batch can never complete: the store machine discards it. A file counts as pushed only once
its batch's journal is complete, so its next push repeats it. A push is split into batches of
about `batch_mb`, so an interruption costs one batch, not the whole push, and the store merges as
batches arrive. All pending deletions go in the first batch.

## The catalog (store machine only)

`%LOCALAPPDATA%\house-drive\catalog.sqlite` on the store machine: every path in `files/` with its
size, hash, modified time and verified flag; every trash entry with its date. At each run it is
written to `catalog.tsv` on the store (staging name, then rename), which the other machines read; a
reader therefore always sees a complete snapshot, never a half-written one.

**Decided while writing step 4 (2026-10-18):**

- **Listed means verified:** the store machine hashes each incoming file on its own disk before
  moving it into `files/`, so the catalog never holds an unverified file. A mismatch is not placed:
  it is recorded as `rejected` (machine, path, hash) in the catalog and its snapshot, so its machine
  pushes it again; a later operation of that machine on that path clears it.
- **The trash on disk is the record:** `trash/<date>/<batch>/<path>`, the date being the day of
  trashing; emptied by folder date (older than `trash_days`), so no table can disagree with it.
- **Refused deletions** are listed (`refused`) until a later operation on the path, or for
  `trash_days`. **Conflicts** are listed until their copy is gone.
- **Repeatable merges:** each operation checks whether an earlier, interrupted run already did it
  (a file already placed, already trashed, a conflict copy already there, found by hash), so the
  catalog is committed per batch and a batch's folders removed only after.
- **Conflict copies are dated by their batch,** not by the merge, so a repeated merge finds the
  same name; ` 2`, ` 3` follow if taken.
- **An edit to a file deleted elsewhere is kept:** a `put` whose path the store no longer has is
  placed, whatever its `base`.

## Each machine's state

`%LOCALAPPDATA%\house-drive\state.sqlite`: every local file with size, modified time, hash, pinned,
pushed, verified, and last use. Hashes are recomputed only when size or modified time changed.

**Last use, for eviction:** phase 1 cannot see a file being opened (phase 3's Explorer
integration can). It takes the later of the file's last access time, where Windows keeps it, and its
last fetch or modification. Coarse, and enough to evict what has not been touched for weeks.

## `sync`, on every machine

1. Load the configuration. Not on the home network (the current network not Private, or its ID not
   `home_network_id`): stop, sending nothing.
2. Connect to `store_root` with the stored credentials; read `store-id.txt`; a mismatch with
   `store_id` stops everything.
3. Scan `data_root` against the state: new and changed files queued for pushing, files deleted
   locally queued as `delete`.
4. Push the queue as one batch: files into `incoming/`, then the journal.
5. Read `catalog.tsv`: mark local files verified where hash and size agree; fetch pinned files that
   are missing or older locally; delete local copies of files deleted elsewhere (never a file with
   unpushed changes: that is a conflict, reported and left alone).
6. Over `cache_limit_gb`: evict least recently used files, verified and unpinned only, down to
   `evict_to_percent`; if not enough can go, warn.

**Decided while writing step 5 (2026-10-18):**

- **A local deletion of a file deleted elsewhere** requires the store to have held this machine's
  very version (its `store_hash`), the copy to be unedited, and unchanged since the scan; an edited
  copy is kept, reported, and pushed back. A file never seen on the store is never touched.
- **A pending local deletion is never fetched back** by a pin.
- **Unedited local copies are refreshed** when the store's version changed elsewhere, pinned or
  not: no local copy goes stale silently.
- **Fetches go through `<data_root>/.house-drive-fetch/`,** renamed into place (same volume),
  keeping the store's modified time; the scan skips that folder, and each run clears leftovers.
- **Eviction candidates:** verified, unedited, unpinned, unchanged since the scan; least recently
  used first. Evicted and remotely deleted files leave the state without queueing a deletion.
- **Pins live in the state** (schema version 2; a version 1 database gains the table).

**Decided while writing step 6 (2026-10-18):**

- **The store machine skips the gate and the connection** (its store is a local folder), so its
  `home_network_id` is optional.
- **`join` trusts the store on first use:** it asks for the share's account, connects, and reads
  the store's ID from `store-id.txt` itself; every later run checks it.
- **An unreachable store** (error codes for a missing path, a gone network, a timeout) ends a run
  quietly, "nothing sent"; a refused connection (wrong password, error 1219 for a conflicting
  connection to the same machine under another account) is an error with its remedy.
- **Passwords are asked without echo** through the console API; mintty, which gives programs a pipe,
  is refused with a pointer to Windows Terminal.
- **`networks`** lists connected networks (profile, ID, name), to check the gate by hand.

## `sync`, on the store machine as well

Before step 5, as the store's sole writer:

- for each complete batch of each machine, in batch order: move each `put` file from `incoming/`
  into `files/` (a replaced file's old version into `trash/`), hash it on the local disk, and mark it
  verified when the hash matches the journal's, or record a mismatch, which asks the machine to push
  again; move each `delete` into `trash/`; then remove the batch's folders;
- a `put` whose hash is already the current one in `files/` is a no-op, never a conflict: a
  machine stopped between writing a journal and recording it pushes the same files again;
- discard a batch whose journal is incomplete once a newer batch of its machine exists;
- empty trash entries older than `trash_days`;
- write `catalog.tsv`.

## Concurrency (decided 2026-10-18)

House Drive is general: it relies on no program's cooperation, though a program that holds files
open without sharing (as Word and Excel do) makes its job easier.

- **A local file being written while pushed.** Opening a file locks it only as far as its opener
  chose (the share mode): many programs share nothing, some share reading or writing, and many
  editors hold no handle at all, rewriting the file in one burst on save. So `sync` does three
  things: it opens each file sharing reading only, so the open fails while anyone holds it for
  writing; it records size and modified time, copies while hashing, and checks both again, dropping
  the copy if either changed; and it leaves files modified in the last minute for the next run.
- **Two runs on one machine** (the scheduled one and one by hand): an exclusive lock file in
  `%LOCALAPPDATA%\house-drive`; the second run quits.
- **Several machines pushing at once:** each writes only its own `incoming/` and `journal/` folders;
  `files/` has one writer, the store machine.
- **A batch still arriving:** merged only once its journal is complete (the `end` line above).
- **`catalog.tsv` read while replaced:** written under a staging name, then renamed over the old one;
  a reader sees one whole snapshot. If the rename fails (the old one open), the next run retries.
- **Fetching or evicting a file in use:** a fetch writes a temporary file on the local disk, then
  renames it into place; an eviction or remote deletion of an open file fails and is retried.
- **Conflicts: two machines editing one file between syncs.** Each `put` carries `base`, the store's
  hash the machine started from. When the store machine merges a `put` whose `base` is not the
  current hash in `files/`, it keeps both: the current version stays, the incoming one lands beside
  it as `<name> (conflict <machine_id> <date>).<ext>`, and `status` reports it on every machine. The
  same check guards a `delete`: a deletion based on an older version than the current one is refused
  and reported, so nothing edited elsewhere vanishes. The user resolves by keeping one and deleting
  the other, as in Google Drive.

## Other commands

- `status`: on the home network or not, store reachable or not, cache use against its limit, files
  pending push, files not yet verified, conflicts, warnings.
- `pin <path>`, `unpin <path>`: a file or a folder.
- `fetch <path>`, `evict <path>`: by hand; `evict` refuses unverified and pinned files.
- `delete <path>`: deletes locally and queues the deletion, which goes to the store's trash.
- `restore <path>`: queues a request the store machine honours by moving the newest trashed version
  back into `files/`.
- `resolve <path> --keep <machine_id>|--keep-both <new name>`: settles a conflict, the version not
  kept going to the trash; the same core actions back phase 2's conflicts window.
- `empty-trash [--older-than <days>]`: on the store machine, empties the trash early; with the
  trash counting toward the quota, as in Google Drive, deleting frees room only once emptied.
- `init-store`: on the store machine, once: creates the layout and `store-id.txt`, prints the ID.
- `join`: on any machine, once, interactive: records `machine_id`, the paths, the store ID, and the
  current network's ID as `home_network_id`; stores the credentials in Credential Manager.
- `schedule`, `unschedule`: register or remove the Task Scheduler task (`schtasks.exe`), every
  `sync_minutes` and at logon, run as the user, never SYSTEM.

The share itself (the dedicated account, SMB3 encryption, NTFS rights that allow creating but not
modifying or deleting, the Private-profile firewall rule) is set up by hand once on the store
machine, from a step-by-step guide in the repository. Automating it needs administrator rights, so
it waits until the manual steps are proven.

## Safety rules, enforced in the core

- Every path read from a journal, the catalog or the share is checked to stay inside its root: no
  absolute paths, no `..`, no drive letters, no alternate data streams.
- A local file is deleted (by eviction or a remote deletion) only when the catalog says its store
  copy is verified with the same hash.
- Any unexpected state stops the run with an error rather than guessing.

## Tests

- **Every path that deletes:** eviction never touches pinned, unverified or unpushed files; a remote
  deletion never removes a file with unpushed changes; trash emptying spares entries younger than
  `trash_days`.
- **Path checks:** `..`, absolute paths, drive letters and streams refused.
- **Interrupted pushes:** a batch without its journal is finished by the next run, never merged.
- **The store machine's merge:** batches in order; a replaced file's old version in the trash; a
  hash mismatch leaves the file unverified and asks again.
- **Concurrency:** a file changed during its copy is not pushed; a journal without its `end` line is
  not merged; a second run quits on the lock.
- **Conflicts:** a `put` on a stale `base` yields a conflict copy, the current version untouched; a
  `delete` on a stale `base` is refused.
- **The catalog snapshot** read back equals the catalog.
- **The network gate:** a wrong network or a wrong store ID sends nothing.
- **An end-to-end run** with two machines and a store as temporary folders on one disk (no SMB):
  push, merge, verify, fetch a pin, evict, delete, restore.

## Steps

1. The workspace, the configuration and its example, path checks.
2. Local state and scanning, with hashing.
3. The store's layout, `init-store`, pushing batches.
4. The store machine's merge, verification, trash and catalog snapshot.
5. Reading the catalog: verified marks, fetching pins, remote deletions, eviction.
6. The network gate and the share connection with Credential Manager; `join`.
7. `status`, `pin`, `fetch`, `evict`, `delete`, `restore`, `resolve`, `empty-trash`.
8. `schedule`; the share's setup guide; a first real installation.

**Added after step 5 (2026-10-18): the global quota,** design.md's "one global quota", as a step of
its own before step 6: `store_quota_gb` (store machine) and a free-space margin; usage and limit in
the snapshot (a `quota` line); merge refusing files past either, listed `over-quota` so their
machines push again later; pushing machines holding new and changed files while the snapshot says
full, deletions still pushed.

## Open, for the developer

1. **Folder pins, decided (2026-10-18): as Google Drive.** The pin belongs to the folder, so files
   added to it later, on any machine, are pinned too and fetched at the next sync. The state records
   pinned paths (files or folders), and a file is pinned when it or any ancestor folder is.
2. **Remote deletions, decided (2026-10-18): as Google Drive.** A file deleted on one machine is
   deleted on the others too, its last version kept in the store's trash for `trash_days`; a copy
   with unpushed changes is never deleted, but reported as a conflict.
3. **A file larger than the cache's limit, decided (2026-10-18): no special rule.** The cache is an
   ordinary folder, its limit enforced by eviction, not a disk's size; the eviction rules already
   cover it (never unverified, never pinned, `status` warns when over the limit).

All three answered: the plan is ready to implement, step 1 first.
