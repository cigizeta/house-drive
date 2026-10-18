# House Drive: design

Design in progress, nothing implemented. Started 2026-10-18.

**In one paragraph:** a few machines in one home, each with a local cache of one shared tree whose
full copy lives on one disk of a store machine, like laptops sharing one Google Drive. Each machine
pushes its new files to the store over the home LAN when the store machine is on, from a program run
by Task Scheduler every few minutes and at startup; files needed offline are pinned; local copies are
evicted only once verified on the store. The store machine is not always on, and a machine may be
away from the home network, so everything works offline and syncs when it can.

**Where to resume:** the design's first pass is complete (2026-10-18); next, the implementation
plan, in a document of its own.

## Decided (2026-10-18)

- **The disks at hand, no purchase.** The first installation: three machines with a 1 TB SSD each,
  and a 1 TB HDD (about 930 GiB usable) in one of them as the store.
- **Each machine's data folder at most 256 GB,** a cache of the store, as Google Drive for desktop
  streams a shared drive into a local cache.
- **The store is a superset:** every file on any machine also lives there, plus files only it holds.
  One tree, each file stored once. Many files are shared between the machines, so the union is far
  below the sum of the caches.
- **Big files are write-once:** copied once, verified, never in conflict. Small mutable ones are
  copied whole, the latest winning, each with one writer: no two machines write the same file.
- **Push:** each machine copies its own new files to the store over the home LAN (an SMB share),
  since the writer knows when a file is complete. The store machine writes to its own disk locally.
- **The store is often off, and machines travel.** Every machine works from its own disk, pushes are
  queued until the store is reachable, and what a machine needs away from home is **pinned**
  beforehand. The store lags behind by whatever was made while it was unreachable; the tool says so.
- **A program with commands, not a virtual drive:** with the store often off, a drive that fetches
  files on open would often fail. Commands: `sync` (push the queue, fetch the pinned, verify),
  `status` (what is only local, only on the store, how full the cache is), `pin`, `evict` (local
  copies already verified on the store only), `fetch`, `delete` (to the trash, security point 5),
  `restore` (from it). A Google Drive-like virtual drive (Windows'
  Cloud Files API) stays possible later on the same store.
- **`house-drive sync` runs by itself on each machine, every N minutes and at startup,** from
  Windows' Task Scheduler; a run with the store unreachable quits at once.
- **AGPLv3, public from the start.** Nothing about any real home is ever committed (CLAUDE.md).
- **Rust,** over C#, Go and Python: one self-contained `.exe` for Task Scheduler with no runtime to
  install, the `windows` crate reaches every Windows API (and a crate wraps the Cloud Files API), and
  the strict compiler suits a tool whose worst bug deletes a last copy.

## Security (decided 2026-10-18)

All six accepted, point 5 as a trash.

1. **No secrets or home details in the repository:** a local, gitignored configuration per machine,
   with a committed example holding placeholders.
2. **Credentials in Windows Credential Manager,** never in a file; the share reached with a dedicated
   local account on the store machine that can reach the store's folder only: never the user's own
   account, never guest access.
3. **SMB3 with encryption required on the share;** the store machine's firewall allows it from the
   home network only (Windows' "Private" profile).
4. **Away from home, a machine neither offers nor seeks the share:** a foreign network on the
   "Public" profile, where Windows' firewall blocks SMB, and `sync` trying only on the recognised home
   network. **Recognised in layers (decided 2026-10-18):** (a) the gate: the current network is on
   the Private profile and its Windows network ID (Network List Manager) is the one recorded in the
   machine's local configuration at setup; anywhere else `sync` quits at once, sending nothing. The
   Wi-Fi name alone was rejected: anyone can give a network the same name. (b) The confirmation: once
   connected, the store's own ID, a random value written when the store was created, must match the
   configured one; a mismatch stops everything, no push and no delete. (c) Encryption and the
   dedicated account (points 2 and 3) remain the real protection, so a perfect imitation of the home
   network still gets nothing readable. Both IDs live only in the local configuration.
5. **Against a compromised machine (ransomware) and mistakes: a trash, as Google Drive's.**
   **Decided (2026-10-18), in place of an append-only store:** deleting is allowed from any machine,
   as Google Drive allows it, but a deleted file moves to a trash on the store, and an overwritten
   file leaves its previous version there too. Only the store machine empties the trash, itself,
   after 30 days, from its own scheduled task, or earlier by hand, locally; the account the other
   machines push with cannot remove anything for good (NTFS rights). A restore within the 30 days is
   one command. So a compromised machine can at worst move everything to the trash, which is
   restored; the cost is the space deleted files keep for 30 days.
6. **Defensive code:** every path from the index or the share checked to stay inside the data root (no
   `..`); a local file deleted only after its copy on the store verifies by content hash; few
   dependencies; run as the user, never SYSTEM.

- **The minutes between syncs: 10 by default, in each machine's configuration file, never
  hard-coded** (2026-10-18). A run with the store off only checks the network and quits, so a short
  interval costs nothing. The scheduled task is registered from the configuration, so changing the
  value and registering again is all a change takes. Likewise every other number a user may want to
  change (the cache's 256 GB, the trash's 30 days) lives in the configuration, with those defaults.

- **At the cache's limit, automatic eviction, as Google Drive** (2026-10-18): past the limit, `sync`
  evicts the least recently used files until the cache is back below a threshold, 90% of the limit
  by default, in the configuration. Never evicted: pinned files, and files whose copy on the store is
  not yet verified, so nothing that exists only on one machine ever vanishes. If the store has been
  off long enough for unpushed files to fill the cache, nothing can be evicted: the cache may pass its
  limit, and `status` warns loudly instead of deleting. Eviction by hand (`evict`) stays.

- **Verification by content hash, computed by the store machine** (2026-10-18): BLAKE3, a few GB a
  second, far faster than the network. The pushing machine sends each file with its own hash; the
  store machine, which runs House Drive too, hashes the copy on its own disk at its next run and
  marks it verified when the two agree, or reports a mismatch and asks for the file again. Each file
  crosses the network once, and corruption is caught, not only truncation. A machine evicts only
  files marked verified, so the few minutes before the mark cost nothing.

- **The index, in two kinds** (2026-10-18):
  - **Each machine's own state,** on the machine, written by it alone: what it holds, what is
    pinned, when each file was last used (for eviction), what waits to be pushed.
  - **The store's catalog, written by the store machine alone,** on its local disk: every file, its
    hash, verified or not, the trash and old versions. A pushing machine writes small journal files
    ("added X with hash H", "deleted Y") into its own folder on the store, creating them and never
    editing them; the store machine merges them into the catalog at each run. No database is shared
    over SMB, which is unsafe with several writers (SQLite among others), and the machines only ever
    create files, as the permissions require.

## Open

Nothing left in the design's first pass. Next: the implementation plan.
