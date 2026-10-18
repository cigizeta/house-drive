# TODO

Loose, running list of outstanding action items -- not full designs (those live in
`active/`/`done/`/`archived/`), just things not to forget.

## Now

- **Phase 1 (phase-1-plan.md): code complete through step 8, 2026-10-18; the trial installation
  is paused halfway, to resume.** A store machine (store, cache of 1 GB, quota of 4 GB, all on its
  SSD) and one other machine (cache of 1 GB), new, empty folders only.
  - **Done on the store machine:** `init-store`, `join`, a first `sync` (a test file pushed,
    placed, verified), the account, the folder's rights, the encrypted share, the Private profile
    and the firewall rules (guide steps 3 to 6). The other machine reaches it (`ping`, Explorer).
  - **Resume with the other machine:** clone and build, `join` (from Windows Terminal, for the
    password), `cache_limit_gb = 1`, `pin` the test file, `sync`, `schedule`; check that deleting
    from the share fails (guide step 8); then the trial's list. Check whether the store machine's
    task is scheduled (`schtasks /Query /TN "House Drive"`); it may have been paused.
  - **Learned so far:** `netsh` cannot change a rule group's profile (the guide now uses
    PowerShell, by port and protocol); Git Bash does not expand `%LOCALAPPDATA%` (use
    `$LOCALAPPDATA`); `join` writes every setting, so edit values rather than adding lines.
  - **Small fixes noticed:** `sync` on the store machine prints the store's usage twice.
  - Then move phase-1-plan.md to `done/` and start phase 2 (the tray and the conflicts window).
