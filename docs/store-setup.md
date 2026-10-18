# Setting up House Drive

This guide sets up a store on one machine of a home network and joins the other machines to it.
Every name below is a placeholder; use your own:

| Placeholder | Meaning |
|---|---|
| `store-machine` | the machine whose disk holds the store |
| `D:\house-drive` | the store's folder on that machine's disk |
| `house-drive` | the share's name |
| `house-drive-push` | the dedicated account the other machines connect with |
| `C:\example\data` | a machine's data folder (its local cache) |

Commands are for Git Bash. Steps marked **(administrator)** need Git Bash started with "Run as
administrator". Git Bash rewrites arguments that start with `/`, so administrative commands are
prefixed with `MSYS_NO_PATHCONV=1`.

## 1. Build and install the programs

On each machine, from the repository:

```bash
cargo build --release
```

`target/release` then holds `house-drive.exe` and `house-drive-sync.exe`. `house-drive schedule`
(step 7) copies both into `%LOCALAPPDATA%\house-drive\bin`.

## 2. Create the store (store machine)

As your ordinary user, in a new, empty folder on the store's disk:

```bash
target/release/house-drive.exe init-store 'D:\house-drive'
```

## 3. The dedicated account (store machine, administrator)

The other machines connect with this account, never with your own, and never as a guest. The `*`
makes `net user` ask for the password instead of leaving it in the shell's history:

```bash
MSYS_NO_PATHCONV=1 net user house-drive-push '*' /add /passwordchg:no
```

```bash
powershell.exe -NoProfile -Command 'Set-LocalUser -Name house-drive-push -PasswordNeverExpires $true'
```

Optionally, hide it from the sign-in screen:

```bash
MSYS_NO_PATHCONV=1 reg add 'HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList' /v house-drive-push /t REG_DWORD /d 0 /f
```

## 4. The folder's rights (store machine, administrator)

These rights are what protect the store from a compromised machine. The account the other
machines use may **read** the store (to fetch files and read the catalog), may **create and write**
only in `incoming\` and `journal\` (to push), and may **delete nothing anywhere**. Only you, on the
store machine, move files into `files\`, trash them and empty the trash.

Inherited rights are removed first. `*S-1-5-18` is SYSTEM and `*S-1-5-32-544` the Administrators
group, written as SIDs because their names differ between Windows languages:

```bash
MSYS_NO_PATHCONV=1 icacls 'D:\house-drive' /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' "$USERNAME:(OI)(CI)F" 'house-drive-push:(OI)(CI)RX'
```

```bash
MSYS_NO_PATHCONV=1 icacls 'D:\house-drive\incoming' /grant 'house-drive-push:(OI)(CI)W'
```

```bash
MSYS_NO_PATHCONV=1 icacls 'D:\house-drive\journal' /grant 'house-drive-push:(OI)(CI)W'
```

A limit worth knowing: Windows cannot grant "create files but never write them again", so the
account can rewrite its own batches still waiting in `incoming\` and `journal\`. That is harmless:
the store machine checks every file against its journal before placing it, and the account can
never touch `files\`, the trash or the catalog, nor delete anything. All machines share the
account, so one could also write into another's waiting batches, with the same harmless result.

## 5. The share, encrypted (store machine, administrator)

```bash
MSYS_NO_PATHCONV=1 net share 'house-drive=D:\house-drive' /grant:house-drive-push,CHANGE "/grant:$USERNAME,FULL"
```

Require encryption, so nothing crosses the network readable:

```bash
powershell.exe -NoProfile -Command 'Set-SmbShare -Name house-drive -EncryptData $true -Force'
```

The share's own permissions allow changes; the folder's rights above are what restrict them.

## 6. The firewall: home network only (store machine, administrator)

In Windows' settings, make sure the home network's profile is **Private** on every machine
(Settings, Network & internet, the connection's properties). Then allow file sharing on the
Private profile only:

```bash
netsh advfirewall firewall set rule group="File and Printer Sharing" new enable=yes profile=private
```

The group's name is the English one; on a Windows in another language, use its translated name,
as shown in "Windows Defender Firewall with Advanced Security". On networks set to Public (a café,
an office), Windows then neither offers nor reaches the share, and House Drive does not try.

## 7. Join each machine

On the store machine first, then on each other machine, at home:

```bash
target/release/house-drive.exe join
```

`join` asks for the machine's ID (for example `machine-a`), whether it is the store machine, its
data folder, and the store's folder (on the store machine) or share (`\\store-machine\house-drive`,
on the others). On the others it also records the home network, asks for `house-drive-push`'s
password, checks it by connecting, keeps it in Windows Credential Manager, and reads the store's ID
from the store itself. Run `join` from Windows Terminal: Git Bash's own window (mintty) cannot hide
a password as it is typed.

Optionally, on the store machine, add a quota to `%LOCALAPPDATA%\house-drive\config.toml`:

```toml
store_quota_gb = 768
```

Then, on every machine:

```bash
target/release/house-drive.exe sync
```

```bash
target/release/house-drive.exe schedule
```

## 8. Check the protection

From a machine other than the store machine, deleting from the share must fail:

```bash
rm '//store-machine/house-drive/store-id.txt'
```

Expected: `Permission denied`. If the file is deleted, the rights of step 4 are wrong: restore the
file from the store machine's own copy (or run `init-store` again in a new folder) before going
further.

## A trial first

Before trusting House Drive with anything that matters, try it with folders that hold nothing of
value: a new, empty data folder on each machine, and a store folder of its own with a small quota.
Things worth trying:

- a file created on one machine appears in the store's catalog, then on another machine once
  pinned (`house-drive pin`) or fetched (`house-drive fetch`);
- `house-drive status` on each machine;
- a file edited on two machines between syncs makes a conflict copy, settled with
  `house-drive resolve`;
- `house-drive delete`, then `house-drive restore`;
- a machine away from home (or on a Public network) sends nothing;
- the store machine off: the others keep working, and catch up once it is back.

## Undoing it

On each machine:

```bash
target/release/house-drive.exe unschedule
```

On the store machine (administrator), the share and the account:

```bash
MSYS_NO_PATHCONV=1 net share house-drive /delete
```

```bash
MSYS_NO_PATHCONV=1 net user house-drive-push /delete
```

Your files stay where they are: in each data folder, and in the store's `files\`.
