# TODO

Loose, running list of outstanding action items -- not full designs (those live in
`active/`/`done/`/`archived/`), just things not to forget.

## Now

- **Phase 1 (phase-1-plan.md): code complete through step 8, 2026-10-18; the trial installation
  restarts with the store on the HDD machine** (decided 2026-10-19), as the design intends: store
  on its HDD, its cache on its SSD, new, empty folders. This store is the real one, quota 768 GB
  (three caches of 256 GB) from the start; only copies, never originals, until the trial passes.
  - **First trial store (abandoned):** set up on an SSD-only machine, guide steps 2 to 7 done there
    with one test file. That machine now joins the HDD store as an ordinary machine: `unschedule`,
    then remove its share, account and firewall rules (guide "Undoing it"), and `join` with a new,
    empty data folder.
  - **On the HDD machine, from scratch:** the Private profile, then guide steps 1 to 7 (store on
    the HDD, data folder on the SSD), `store_quota_gb = 768`, `sync`, `schedule`.
  - **Done on the HDD machine (2026-10-19):** setup and first sync, schedule; delete then restore
    (the trash and back); automatic eviction (12 files of 100 MiB in a 1 GB cache: the 5 least
    recently used evicted, back under 90%); the run lock. A scheduled run starved for minutes
    behind a continuous CPU load at normal priority: the task runs below normal (Task
    Scheduler's default, 7). The load was fixed on its own side; whether House Drive's task should
    run at normal priority anyway stays open.
  - **Then the other machines:** `join`, `sync`, `schedule`; check that deleting from the share
    fails (guide step 8); then the trial's list.
  - **Learned so far:** `netsh` cannot change a rule group's profile (the guide now uses
    PowerShell, by port and protocol); Git Bash does not expand `%LOCALAPPDATA%` (use
    `$LOCALAPPDATA`); `join` writes every setting, so edit values rather than adding lines.
  - **Small fixes noticed:** `sync` on the store machine prints the store's usage twice. Sizes
    print in decimal GB while Explorer shows binary units labelled "GB", so the same disk reads
    996 GB in House Drive and 928 GB in Explorer: to decide (proposed: match Explorer, for printed
    sizes and the configured GB values alike). A scheduled run writes `sync.log` only when it
    ends, so a long or stuck run is invisible (no log, `sync` says only "another run is in
    progress"): `status` could say a run is in progress and since when, and the tray (phase 2)
    must show it.
  - Then move phase-1-plan.md to `done/` and start phase 2 (the tray and the conflicts window).
