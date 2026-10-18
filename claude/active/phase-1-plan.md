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
A batch's journal is written after its files, and ends with an `end<TAB><line count>` line: a batch
without a complete journal is an interrupted push, which the next run of its machine finishes, and
which the store machine never merges. (Writing under a temporary name and renaming is not possible:
pushing machines may only create.)

## The catalog (store machine only)

`%LOCALAPPDATA%\house-drive\catalog.sqlite` on the store machine: every path in `files/` with its
size, hash, modified time and verified flag; every trash entry with its date. At each run it is
written to `catalog.tsv` on the store (staging name, then rename), which the other machines read; a
reader therefore always sees a complete snapshot, never a half-written one.

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

## `sync`, on the store machine as well

Before step 5, as the store's sole writer:

- for each complete batch of each machine, in batch order: move each `put` file from `incoming/`
  into `files/` (a replaced file's old version into `trash/`), hash it on the local disk, and mark it
  verified when the hash matches the journal's, or record a mismatch, which asks the machine to push
  again; move each `delete` into `trash/`; then remove the batch's folders;
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
7. `status`, `pin`, `fetch`, `evict`, `delete`, `restore`, `resolve`.
8. `schedule`; the share's setup guide; a first real installation.

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
