# Lab instance control

fakescan runs as a **single stable process per port**.  Windows PnP / WIA keys
the scanner association to the advertise URL + UUID.  Restarting with different
args forces `remove` + re-Add (manual step).  This doc explains how to keep the
process alive and use the control surface instead.

## Why stable identity matters

The first time a person completes **Add device** in Settings → Printers & scanners,
Windows writes a PnP association for `<uuid> → <advertise-url>:<port>`.  If the
process restarts with a different port or UUID, the association becomes stale,
WIA returns `0x80210001` (Connect) or `0x80004005` (retrieve), and the person
must do Imaging `remove` + re-Add.

**Rule:** prefer `/v1/*` reconfigure over kill/restart. Keep `fakescan.exe`
**unelevated**; Imaging remove uses `.\scripts\fakescan.ps1 helper` (one UAC) or
an elevated shell — see [windows.md](windows.md#helper--elevated--remove--rebind).
SOAP / control / local matrix never need elevation. Add device is still a person.

## Port band

fakescan uses **even ports: 3920, 3922, 3924, …**

Odd ports (3921, 3923, …) are reserved for fakeprint.  The registry and `add`
command enforce this.  Running two fakescans on 3920 + 3922 is fine; the sibling
fakeprint on 3921 is unrelated.

## Device registry

`%LOCALAPPDATA%\fakescan\devices.json`, or the file named by **`FAKESCAN_REGISTRY`** (honored by
`fakescan.exe`, `fakescan.ps1` and the tests; every test points it at a temp file and never
touches the real one). One row per emulated device; rows with the same `listen` belong to one
process. Only daemon-style runs (`--manage` or `--lab-name`) attach the registry; embedded
in-process servers never read or write it. Old single-row files load unchanged (`added.pnp` is
still read, and kept in sync with `install.state == "installed"`). Unknown fields are preserved
on rewrite. Writes are atomic (temp file, fsync, rename) under an in-process mutex plus an OS
lock on `devices.json.lock`. The PowerShell writers (`add`, `remove`, `mark-added`) take the same
lock (`LockFileEx` whole-range on that file, ~5 s retry) around read-modify-write; while the
daemon is running prefer its HTTP/ctl surface anyway.

Row shape (every new field is optional on read):

```jsonc
{
  "name": "tray5",
  "listen": "0.0.0.0:3920", "advertise": "192.168.1.70",
  "device_uuid": "00000000-0000-4000-8000-0000000000a5",
  "profile": "feeder-tray5",
  "friendly_name": "fakescan tray5",
  "config": {
    "fingerprint": "sha256:...",            // everything effective (capacity, media, duplex, formats, dpi, colors, pages)
    "published_fingerprint": "sha256:...",  // only what Windows caches from GetScannerElements
    "feeder": { "capacity": 5, "media_sizes": ["letter"], "duplex": false }
  },
  "install": {
    "state": "installed",                   // new | probed | installed | stale
    "reason": null,                         // why it is stale
    "first_probed_at": "2026-10-12T17:50:01Z", "last_seen_at": "...", "last_peer": "192.168.1.40",
    "installed_at": "...", "verified_at": "...",
    "wia": { "id": "{6BDD1FC6-...}\\0003", "name": "fakescan tray5",
             "port": "urn:uuid:<uuid>/uri:<uuid>/ScannerService" },
    "published_fingerprint_at_install": "sha256:..."
  },
  "added": { "pnp": true }                  // legacy mirror of install.state == installed
}
```

Install states:

- `new`: nothing has talked to it. `probed`: fakescan saw an HTTP/SOAP request or a WS-Discovery
  Probe/Resolve from a peer (a Resolve marks only the addressed device; a Types-matching Probe
  marks every device that answers it). `last_seen_at` is rewritten at most once per ~30 s per device.
- `installed`: a Windows WIA device bound to this UUID was observed, reported by
  `POST /v1/devices/{id}/installed`, ctl `installed`, or `POST /v1/reconcile`.
- `stale`: it was installed, but either `reconfigure` changed the **published** capabilities
  (ADF max size from `media_sizes`, duplex, formats, dpi, colors, glass vs feeder; tray capacity
  and runtime state do not count), or reconcile no longer finds it (`not listed by Windows`).
  Windows caches GetScannerElements at Add time, so remove and re-Add in Imaging.
  `GET /v1/devices` shows `needs_readd` + `needs_readd_reason`.
  A `profile changed` stale heals back to `installed` if the profile is switched back so the
  published capabilities equal the install-time ones again (also across a daemon restart);
  `not listed by Windows` only clears via a reconcile. The tray waits 20 s before alerting about
  a `profile changed` stale, so a quick switch and switch-back raises no popup.

**Foreign-device rule:** reconcile matches a WIA scanner to a row **only** by the row's device
UUID appearing in the WIA `port` string (case-insensitive; braces and `urn:uuid:` tolerated).
Never by name. WIA entries matching no row are returned as `foreign` and are never touched,
recorded or scanned. `installed` likewise refuses a `wia_port` that lacks the device's UUID.

Legacy `mark-added` (`fakescan.ps1 mark-added`, ctl `mark_added`) sets `installed` with
`note: "legacy mark-added"` and claims no WIA details.

HTTP / ctl surface (thin wrappers over `DeviceManager`): `GET /v1/devices`, `GET /v1/registry`,
`POST /v1/devices/{id}/reconfigure {profile}`, `POST /v1/devices/{id}/installed {wia_id,wia_name,wia_port}`,
`POST /v1/devices/{id}/forget`, `POST /v1/reconcile {scanners:[{id,name,port}]}`,
`DELETE /v1/devices/{id}[?forget=true]` (keeps the row unless `forget=true`); ctl commands
`installed`, `mark_added`, `forget`, `reconcile` on `POST /v1/ctl` (device-scoped under `/d/{id}/v1/ctl`).

`fakescan --manage` (no `--device` flags) brings up every row whose `listen` equals its own, with the
stored UUID and profile, and never rewrites a UUID. `fakescan daemon install|uninstall|status`
(also `fakescan.ps1 daemon ...`) manages a per-user logon scheduled task that runs
`fakescan --manage`; it is not a Windows service.

Example (one instance, old single-row format, still valid):

```json
{
  "port_policy": { "base": 3920, "step": 2 },
  "devices": [
    {
      "name": "default",
      "listen": "0.0.0.0:3920",
      "advertise": "192.168.1.70",
      "device_uuid": "00000000-0000-4000-8000-0000000000f2",
      "profile": "brother-wsd",
      "exe": "D:\\temp\\fakescan-target\\debug\\fakescan.exe",
      "autostart": true,
      "added": { "pnp": true }
    }
  ]
}
```

## Quick start (PowerShell)

```powershell
# List all registered instances
.\scripts\fakescan.ps1 list

# Add the default instance (auto-picks listen port 3920)
.\scripts\fakescan.ps1 add -Name default -Exe D:\temp\fakescan-target\debug\fakescan.exe

# Ensure the default instance is running (attach-first; never kills)
.\scripts\fakescan.ps1 ensure

# Ensure all autostart rows
.\scripts\fakescan.ps1 ensure -All

# Send a ctl command
.\scripts\fakescan.ps1 ctl pages '{"count":3}'
.\scripts\fakescan.ps1 ctl reset
.\scripts\fakescan.ps1 ctl save   # write running state to registry

# Mark as WIA-added (after person clicks Add device)
.\scripts\fakescan.ps1 mark-added

# Remove registry row (does NOT kill the process)
.\scripts\fakescan.ps1 add -Name bpl0 -Listen 0.0.0.0:3922
```

## Quick start (Python)

```bash
pip install -e ../fakescan-python        # base package

fakescan-ctl list
fakescan-ctl add --name default --exe D:\temp\fakescan-target\debug\fakescan.exe
fakescan-ctl ensure --name default
fakescan-ctl pages 3
fakescan-ctl ctl reset
fakescan-ctl save
fakescan-ctl mark-added --name default
```

## GET /v1/ctl — capabilities + identity

```bash
curl http://127.0.0.1:3920/v1/ctl
```

```json
{
  "cmds": ["status","health","profile","pattern","pages","feeder",
           "jpeg_com","effects","scenario","fault","slow",
           "loopback","reset","save"],
  "port": 3920,
  "uuid": "00000000-0000-4000-8000-0000000000f2",
  "base_url": "http://127.0.0.1:3920",
  "name": "default"
}
```

## POST /v1/ctl — unified command

All existing `/v1/*` endpoints remain for backward compat.  `/v1/ctl` is a
unified front-end over the same handlers.

```bash
curl -X POST http://127.0.0.1:3920/v1/ctl \
  -H "content-type: application/json" \
  -d '{"cmd":"pages","args":{"count":3}}'

curl -X POST http://127.0.0.1:3920/v1/ctl \
  -d '{"cmd":"reset"}'

curl -X POST http://127.0.0.1:3920/v1/ctl \
  -d '{"cmd":"save"}'
```

### `save` command

Writes the running process's current state (listen, uuid, profile) back to its
devices.json row.  Requires `--lab-name` (or `FAKESCAN_NAME`) to be set when
the process started.

```bash
# Start with name
fakescan --lab-name default --advertise 192.168.1.70
# After changing profile:
curl -X POST http://127.0.0.1:3920/v1/ctl -d '{"cmd":"profile","args":{"id":"brother-usb"}}'
curl -X POST http://127.0.0.1:3920/v1/ctl -d '{"cmd":"save"}'
```

## Windows named pipe (Windows only)

`\\.\pipe\fakescan-<name>` (default `\\.\pipe\fakescan-default`)

JSONL wire protocol — same schema as `/v1/ctl`:

```text
→ {"cmd":"pages","args":{"count":3}}\n
← {"ok":true,"result":{...status...}}\n
```

Use `fakescan-ctl pipe pages --args '{"count":3}'` or connect with any JSONL
tool.  Non-Windows: use HTTP only.

## MCP (attach-only)

```bash
pip install -e "../fakescan-python[mcp]"   # installs mcp package
fakescan-mcp --base-url http://127.0.0.1:3920
```

Cursor / Claude MCP config entry (stdio):

```json
{
  "fakescan": {
    "command": "fakescan-mcp",
    "args": ["--base-url", "http://192.168.1.70:3920"]
  }
}
```

The MCP server **never starts or stops fakescan**.  It fails fast (error tool
result) if the device is not reachable.  Available tools: `ctl_info`, `status`,
`reset`, `pages`, `pattern`, `scenario`, `fault`, `ctl`.

The package above is the external Python server and is not in this tree. The
in-repo server is the optional `fakescan-mcp` crate (`docs/proposals/mcp.md`):
stdio JSON-RPC, `send_*` confirm-gated, `write_*` refuses to
overwrite, everything else read-only.

## Running multiple instances

fakescan supports parallel named instances **within the same role**:

| Name | Port | UUID (last 4) |
|------|------|---------------|
| `default` | 3920 | `…00f2` |
| `bpl0`    | 3922 | `…00f4` |

Each needs a distinct port (even band) and UUID.  Each requires one person
Add-device pass.

```powershell
.\scripts\fakescan.ps1 add -Name bpl0 -Listen 0.0.0.0:3922 `
    -DeviceUuid 00000000-0000-4000-8000-0000000000f4 `
    -Exe D:\temp\fakescan-target\debug\fakescan.exe
.\scripts\fakescan.ps1 ensure -Name bpl0
```

## Reboot persistence

1. Add the exe path to the registry row (`add -Exe PATH`).
2. Set `autostart = true` in the row (`add -Autostart`).
3. Install a Windows logon task:

```powershell
$a = New-ScheduledTaskAction -Execute fakescan.exe `
     -Argument "--lab-name default --listen 0.0.0.0:3920 --advertise 192.168.1.70"
$t = New-ScheduledTaskTrigger -AtLogOn
Register-ScheduledTask -TaskName FakescanLab -Action $a -Trigger $t -RunLevel Highest
```

Or use `ensure` in a startup script; it does nothing if the process is already
running. `ensure` starts ONE `fakescan --manage --listen L ...` per distinct `listen` (that
process brings up every registry row on L with its stored UUID), attaches instead when
`/health` on L answers, and never kills anything. For the supported per-user logon task see
"Operating the daemon" below.

## Operating the daemon

The daemon is one `fakescan --manage` process hosting every device in the registry; Windows keeps
the Imaging devices you Added across restarts as long as the UUIDs, URLs and published
capabilities stay the same.

**Install the logon task** (per user, unelevated; not a Windows service):

```powershell
fakescan.exe daemon install --profiles D:\code\fakescan\profiles --listen 0.0.0.0:3920 --advertise 192.168.1.70
fakescan.exe daemon status      # shows the scheduled task `fakescan-daemon`
fakescan.exe daemon uninstall
```

The task runs `fakescan --manage --profiles <abs> [--listen ..] [--advertise ..]` at logon.
`install` replaces an existing task of the same name. (Tested: the command-line construction;
the real `schtasks` calls are not exercised by the automated tests.)

**Check status:** `.\scripts\fakescan.ps1 list` (registry, with install state) or
`GET /v1/devices` (live; includes `install`, `config`, `needs_readd`).

**A human Adds a class in Imaging** (Settings > Printers & scanners > Add device, or
`fakescan.ps1 install`):

1. Create the device if it is not there: `POST /v1/devices {"id":"tray5","profile":"feeder-tray5"}`
   (or `add` + restart-free `ensure`). The UUID is allocated once and stored.
2. `GET /v1/devices` and read the device's `uuid`, `device_endpoint` (XAddr, the URL Windows
   binds to) and `friendly_name`. Add that scanner in Imaging; confirm the new Windows scanner's
   WIA port string contains the UUID.
3. Reconcile: `fakescan.registry.reconcile(wia_scanners)` (Python), `POST /v1/reconcile`, or ctl
   `installed {device, wia_id, wia_name, wia_port}`. Matching is by the UUID inside the WIA
   `port` string only, never by name; the row becomes `installed` and records the published
   fingerprint at install. A WIA scanner whose port UUID is not in the registry is reported as
   foreign and must not be scanned.

**Do not restart or re-profile an installed device to "fix" things.** A restart with the same
listen, UUIDs and profiles is invisible to Windows, but a different identity is a different
device. Windows caches GetScannerElements at Add time: changing a profile's *published*
capabilities (ADF size/media sizes, duplex, formats, dpi, colors, glass vs feeder) via
`/v1/profile`, ctl `profile` or `reconfigure` marks the row `stale` (`needs_readd`) and you must
remove and re-Add it in Imaging. Capacity, pages loaded, faults, loopback images and other
runtime state never need a re-Add; tests should set them and reset them in teardown instead of
swapping profiles.

## Install-attention UX

When interactive Add device is still required (`added.pnp = false` and no
silent install has succeeded), `Open-FakescanAddDevice` fires a multi-channel
alert via the private `Invoke-LabAttention` helper.

### Default (no flag)

1. **Yellow console banner** — always printed; survives output redirection.
2. **`[Console]::Beep(1000, 300)`** — 1 kHz tone for 300 ms.
3. **BEL (`[char]0x7`)** — emitted alongside the beep.
4. **WinForms `NotifyIcon.ShowBalloonTip`** — system-tray balloon, held 3 s then
   disposed.

### Flags

| Flag | Effect |
|------|--------|
| *(default)* | Banner + beep + BEL + balloon |
| `-Loud` | All of the above **plus** `FlashWindowEx` on the first parent-process window handle found by walking the process tree. |
| `-Quiet` | **Banner only** — beep and balloon suppressed. |

### Where it fires

- **`ensure`** — only after quieter paths fail. Order when `added.pnp` is false:
  1. Already in WIA → `mark-added` (no UI)
  2. Elevated → `fakescan.ps1 associate` (`IPNPXDeviceAssociation`); mark-added on success
  3. Else attention + Settings (`Open-FakescanAddDevice`)
- **`install`** — when no silent path (PairAsync) has produced `InstalledAfter = true`.
- **`rebind`** — after the remove + AEP-unpair sequence (and fdPHost/DAS bounce on Unpair AccessDenied), before opening Settings.
- **`add-device`** — direct command.

Older binaries without `GET /v1/ctl` still attach via `/health` + `/v1/status`.

### Examples

```powershell
# Default attention (banner + beep + balloon)
.\scripts\fakescan.ps1 ensure

# Beep-free for shared offices
.\scripts\fakescan.ps1 install -BaseUrl http://192.168.1.70:3920 -Quiet

# Flash terminal window as well (good for background automation)
.\scripts\fakescan.ps1 rebind -BaseUrl http://192.168.1.70:3920 -Loud

# Direct attention signal without running install flow
.\scripts\fakescan.ps1 add-device -Loud
```

## Device manager UI

Open `http://127.0.0.1:<port>/ui` (the daemon default is `http://127.0.0.1:3920/ui`). The page is one
embedded HTML file (no external requests) and is **loopback-only**: any non-loopback peer gets `403`.
It polls `GET /v1/devices` every 2 s (paused while the tab is hidden) plus `GET /d/{id}/v1/status` per
device. Each action is a plain call to the same API the CLI and Python use:

| UI action | Call |
|-----------|------|
| Profile picker | `GET /v1/profiles` (id, connection, formats, feeder capacity/media/duplex, description) |
| Add device | `POST /v1/devices {id, profile, device_uuid?, friendly_name?}` (409/400 text is shown) |
| Remove (+ "forget registry row") | `DELETE /v1/devices/{id}` / `?forget=true` |
| Reconfigure profile | `POST /v1/devices/{id}/reconfigure {profile}` (warns when the device is installed) |
| Set pages | `POST /d/{id}/v1/pages {count}` (409 over capacity is shown) |
| Fault toggles (jam, cover_open, multi_feed, lamp_error, busy, offline) | `POST /d/{id}/v1/fault {<name>: bool}` |
| Reset runtime state | `POST /d/{id}/v1/reset` |
| Add device in Windows | `POST /v1/ctl {"cmd":"open_settings","page":"printers"}` -> `{"opened":bool}` |

`open_settings` is Windows-only, accepted from loopback peers only, and `printers` is the only page
allowed (it runs `cmd /C start "" ms-settings:printers` in the daemon's session). Reconcile is not in
the page because the daemon does not enumerate WIA: use `lab reconcile` or Python
`fakescan.registry.reconcile()`.

## Tray icon

On Windows the daemon itself shows a notification-area icon (cargo feature `tray`, on by default for
Windows builds only; `cargo build --no-default-features` and non-Windows builds have none). There is
no separate tray process: if the icon is there, the daemon is running.

**When it starts:** `fakescan --manage` or `--lab-name` on Windows, unless `--no-tray`. Control-only
`--no-wsd` runs, plain `fakescan` with neither flag, the embedded Python `Server`, tests and CI never
start a tray (it is started from `main.rs` only, never from `server::serve`). The logon task from
`fakescan daemon install` runs `--manage`, so it starts the tray at logon. If the icon cannot be
created (for example no interactive desktop) a warning is logged and the daemon carries on headless.

**Icon colour:** green = every device installed; amber = at least one device is `new`, `probed`,
`stale` or `needs_readd` (needs an Add or Re-Add in Printers & scanners); red = the tray cannot read
the daemon. The tooltip summarises it, e.g. `fakescan: 3 devices, 1 installed, 1 need Add, 1 need re-Add`.

**Menu:**

| Item | Action |
|------|--------|
| Open device manager | opens `http://127.0.0.1:<port>/ui` in the default browser |
| `default: installed`, `tray5: needs Add`, `glass: needs re-Add (profile changed)` | one disabled status line per device, rebuilt whenever the state changes |
| Open Add device settings | opens `ms-settings:printers` |
| Quit fakescan > Yes, stop all emulated devices and quit | graceful shutdown (Bye per device); Windows keeps the Added devices as long as listen + UUIDs match on the next start |

**Notifications:** when a device newly enters needs-Add/Re-Add, a hidden `powershell.exe -STA`
helper raises a balloon whose click opens `ms-settings:printers` (same approach as
`Invoke-LabAttention`). At most one per device per 10 minutes, plus one summary at start-up if
anything needs an Add. The tray polls the daemon's own loopback `GET /v1/devices` every 2 s.

## Siblings: fakeprint

fakeprint runs independently on odd ports.  See [siblings.md](siblings.md) for
the co-existence rules.

**Never kill fakeprint processes** from fakescan scripts.  The two registries,
pipes, MCP servers, and port bands are completely separate.
