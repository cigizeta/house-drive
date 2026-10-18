# CLAUDE.md -- House Drive

**House Drive** is a Google Drive lite for the machines of one home: each machine keeps a local cache
of one shared tree, and one disk on the home LAN holds every file. It works offline and syncs when
the store machine is on. Written in Rust, for Windows.

## Never leak secrets or sensitive information

This repository is public: everything committed is visible to anyone, forever, history included.
**Never write**, to any file, commit message, issue, pull request, published page or external
service:

- **Secrets:** passwords, tokens, keys, credentials of any kind, even expired or fake-looking ones.
- **Identifying details:** hostnames, IP addresses, MAC addresses, user names, e-mail addresses,
  share names, paths on a real machine, serial numbers.
- **Personal or household information:** schedules, habits, travel, when a home is empty or a
  machine is off, where anyone works or lives.

Real values live only in each machine's own gitignored configuration; the repository holds an
example with placeholders. Examples in code, tests and docs use generic names (`machine-a`,
`store-machine`, `C:\example\data`). Before writing a file the developer may commit, check it for
any of the above; when in doubt, leave it out and ask. A leak already committed cannot be fixed by a
later commit: say so at once, so the developer can rewrite history and rotate the secret.

## No autonomous writes allowed

The development of this project assumes that access to NotebookEdit, Bash,
and Monitor are fully denied to Claude. As a consequence, Git is also
fully denied.

## claude/ organization

`claude/` (prose, distinct from the `.claude/` harness config) holds `TODO.md` plus `active/`,
`done/`, and `archived/`.

- `active/`: work in progress.
- `done/`: completed, verified work, moved from `active/` as-is, no rename.
- `archived/`: superseded/abandoned/stale docs, moved here as-is with
  a "superseded, see X" note if applicable.

Source code must never reference a `claude/` document (bare filename or
path) -- inline the "why" directly in the code instead.

Within `claude/`, docs may reference each other, but only by bare filename,
because filenames survive moves between the three subdirectories; paths do not.

## Design first, implement later

Do not rush off implementing. First discuss the design thoroughly, then write
the implementation plan as a document in `claude/active/`, and only then apply
it.

## Fenced blocks

When you want the developers to run something themselves, such as shell
commands or a commit, put it inside a fenced block so that they can copy it.
Put each command in its own fenced block: three consecutive commands are
three fenced blocks, never one block holding all three. Commit messages
must be one line only.

## Development platform: Windows only

House Drive is developed, tested and run on native Windows only, not WSL, and Linux is not a
target: no `#[cfg(not(windows))]` twin and no Linux path. The shell is **Git Bash**, never
PowerShell or cmd: every command in a fenced block uses Bash syntax.

- Rust uses the default `x86_64-pc-windows-msvc` toolchain.
- Windows-specific code needs no `#[cfg]` pair, and no new dependency when a few lines suffice.
- Line endings are governed by `.gitattributes`: Bash scripts always LF, Windows scripts always CRLF,
  every other text file stored LF and checked out with the OS's native endings.
- Git Bash rewrites arguments that look like Unix paths when calling native Windows programs; prefix
  the command with `MSYS_NO_PATHCONV=1` when that bites.
- Code that launches processes cannot rely on Bash: Rust's `Command` needs the real executable name,
  including `.exe`.

## Safety first

House Drive moves and deletes other people's files. A local file is deleted only once its copy on
the store has been verified; a path read from the index or the store is checked to stay inside the
data root; a failure stops rather than guesses. Tests cover every path that deletes.

## Licensing

House Drive is AGPLv3 only (`LICENSE`, `AGPL-3.0-only`).

## No em dashes

Never write em dashes or en dashes to any file in this repository, Markdown or otherwise. Write a
double hyphen (--) instead. Ordinary hyphens (-) are fine.

## Explicit `return` in Rust

Functions that return a value (`-> T`) use an explicit `return <expr>;` instead of an
implicit tail expression. `()`-returning functions need no `return`. `if`/`match` elsewhere
in a function body (e.g. assigned to a `let`) are unaffected -- this only governs a
function's own tail position.
