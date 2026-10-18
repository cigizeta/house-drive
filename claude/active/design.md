# House Drive: design

Design in progress, nothing implemented. Started 2026-10-18.

**In one paragraph:** a few machines in one home, each with a local cache of one shared tree whose
full copy lives on one disk of a store machine, like laptops sharing one Google Drive. Each machine
pushes its new files to the store over the home LAN when the store machine is on, from a program run
by Task Scheduler every few minutes and at startup; files needed offline are pinned; local copies are
evicted only once verified on the store. The store machine is not always on, and a machine may be
away from the home network, so everything works offline and syncs when it can.

**Where to resume:** the security proposals below (question pending: the store append-only from the
machines' side), then the open list.

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
  copies already verified on the store only), `fetch`. A Google Drive-like virtual drive (Windows'
  Cloud Files API) stays possible later on the same store.
- **`house-drive sync` runs by itself on each machine, every N minutes and at startup,** from
  Windows' Task Scheduler; a run with the store unreachable quits at once.
- **AGPLv3, public from the start.** Nothing about any real home is ever committed (CLAUDE.md).
- **Rust,** over C#, Go and Python: one self-contained `.exe` for Task Scheduler with no runtime to
  install, the `windows` crate reaches every Windows API (and a crate wraps the Cloud Files API), and
  the strict compiler suits a tool whose worst bug deletes a last copy.

## Security, proposed (not yet decided)

1. **No secrets or home details in the repository:** a local, gitignored configuration per machine,
   with a committed example holding placeholders.
2. **Credentials in Windows Credential Manager,** never in a file; the share reached with a dedicated
   local account on the store machine that can reach the store's folder only: never the user's own
   account, never guest access.
3. **SMB3 with encryption required on the share;** the store machine's firewall allows it from the
   home network only (Windows' "Private" profile).
4. **Away from home, a machine neither offers nor seeks the share:** a foreign network on the
   "Public" profile, where Windows' firewall blocks SMB, and `sync` trying only on the recognised home
   network.
5. **Against a compromised machine (ransomware): the store append-only from the machines' side.** The
   push account may create files but neither modify nor delete them (NTFS rights), so malware on one
   machine cannot destroy the store; the mutable files go in as new versions; pruning and tidying only
   on the store machine, locally. **The pending question:** is that acceptable? It shapes the design
   most.
6. **Defensive code:** every path from the index or the share checked to stay inside the data root (no
   `..`); a local file deleted only after its copy on the store verifies by content hash; few
   dependencies; run as the user, never SYSTEM.

## Open

- N, the minutes between syncs (10 suggested: a run with the store off only checks and quits).
- What happens at the 256 GB limit: eviction by hand with help, automatic least-recently-used among
  verified files, or a hard refusal (a fixed-size `.vhdx` would enforce one).
- Verification: a content hash per file, or size and time.
- Where the store's own index lives (which machine holds which file).
