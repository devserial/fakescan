# Architecture

## Goals

1. **Deterministic** — same profile + same page stack → same bitmaps out of WIA.
2. **Two capability surfaces** — “USB-full” vs “WSD-lite”, matching real inbox WSD vs vendor USB.
3. **Scriptable** — CI sets feeder state and pages without a person at the glass.
4. **No app mocks** — pdfcraft talks real WIA; fakescan is the other end of the pipe.

## Components

```
┌─────────────────────────────────────────────────────────────┐
│  fakescan host                                              │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────────────┐ │
│  │ control API │  │ page store   │  │ capability profile │ │
│  │ :3920       │  │ BMP/PNG/JPEG │  │ JSON               │ │
│  └──────┬──────┘  └──────┬───────┘  └──────────┬─────────┘ │
│         │                │                      │           │
│         └────────────────┼──────────────────────┘           │
│                          ▼                                  │
│              ┌───────────────────────┐                      │
│              │ scan engine (job FSM) │                      │
│              └───────────┬───────────┘                      │
│           ┌──────────────┴──────────────┐                   │
│           ▼                             ▼                   │
│  ┌─────────────────┐         ┌────────────────────┐         │
│  │ WSD Scan +      │         │ WIA USD (optional) │         │
│  │ WS-Discovery    │         │ INF + DLL          │         │
│  └────────┬────────┘         └─────────┬──────────┘         │
└───────────┼────────────────────────────┼────────────────────┘
            │ SOAP / HTTP                │ WIA COM
            ▼                            ▼
     WSDScan.sys / WIA            Scanners and Cameras
            └────────────┬───────────────┘
                         ▼
                    pdfcraft / WIA apps
```

## Control API (implemented)

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/health` | process up |
| `GET` | `/v1/status` | profile, feeder, pages, pattern, **effects**, scenario, discovery counters |
| `POST` | `/v1/profile` | switch `brother-usb` / `brother-wsd` / custom |
| `POST` | `/v1/jpeg-com` | `{"enabled":false}` omit JFIF COM markers |
| `POST` | `/v1/feeder` | `{ "ready": true\|false }` — sticky FEED_READY |
| `POST` | `/v1/pages` | feeder sheet count; optional `side`/`order` for **manual** duplex |
| Profile `feeder.duplex` | — | hardware ADF duplex (`ADFDuplex` → front+back faces; see [test-matrix.md](test-matrix.md) §8b) |
| `POST` | `/v1/pattern` | synthetic layout id (`page_id`, `wia_plain`, …); `wia_plain` = minimal baseline JPEG |
| `POST` | `/v1/effects` | image defects (skew, speckles, streaks, hole_punch); see [defects.md](defects.md) |
| `GET` | `/v1/scenarios` | ordered tester progression |
| `POST` | `/v1/scenario` | `{ "id":"order" }` — apply one progression step |
| `POST` | `/v1/slow` | `{ "ms" }` pre-RetrieveImage sleep; optional `{ "bps" }` throttles MTOM TCP send (bytes/sec, Content-Length kept) for mid-page progress labs |
| `POST` | `/v1/fault` | jam / cover_open / multi_feed / lamp_error / busy / temporary / internal / offline / ms / `*_after_n` / abort_retrieve* / `cover_open_after_n` / `lamp_warmup_ms` — see [lab-harness.md](lab-harness.md) |
| `GET` `POST` `DELETE` | `/v1/faults/plan` | scheduled faults (between pages / mid page / timed / now) — see [defects.md](defects.md#fault-plan-interrupt-a-scan-mid-scan-or-between-pages) |
| `GET`/`POST`/`DELETE` | `/v1/loopback` | hard page store (upload JFIF → exact RetrieveImage echo) |
| `POST` | `/v1/reset` | clear jobs, effects, pattern, loopback, feeder defaults |

Default listen is `0.0.0.0:3920` so a LAN client can open the scan service. The control API is on that same port and has no authentication token. Scan knobs (`/v1/pages`, profile, fault, pattern, …) are reachable from any peer on purpose. Commands that stop the process, write `devices.json`, or change the device set and install state (`shutdown`, `save`, `installed`, `mark_added`, `forget`, `reconcile`, `POST`/`DELETE /v1/devices`, `GET /v1/registry`) are accepted from a loopback peer only, the same rule as `/ui` and `open_settings`. The Windows named pipe is local and does not do that HTTP peer check. Profile ids are a single `[A-Za-z0-9_-]` file name under the profiles directory.

**Lab harness** (optional Python client, not a Cargo dep): [lab-harness.md](lab-harness.md) / `python/`. Print companion on `:3921`: [siblings.md](siblings.md).

## WSD path (P0)

Protocol layering and authoritative links: **[specs/](specs/README.md)** (WS-Scan, DPWS, Discovery, Transfer, Eventing, Windows client).

| Step | Status |
|------|--------|
| WS-Discovery Hello (+ ProbeMatch when `:3702` free) | working |
| HTTP Device + `/ScannerService` | SOAP dispatcher |
| `GetScannerElements` / `DefaultScanTicket` | done (profile → encodable WSD FormatValues) |
| `CreateScanJob` / `RetrieveImage` (MTOM) | wire done (~200KB JFIF); wsdscdrv accept still CancelJob — [mtom-retrieveimage.md](specs/mtom-retrieveimage.md) |
| WS-Transfer Get | ThisModel / ThisDevice / Relationship **Host+Hosted** + PnP-X ids |
| WS-Eventing Subscribe / Unsubscribe / Renew | accept; job, condition, status-summary, elements-change, and ScanAvailable events POST to NotifyTo when one is registered |
| Windows Add → WIA Connect → acquire | person Add + Connect **live**; RetrieveImage accept gap — [e2e-wia-status.md](e2e-wia-status.md) |

Reference prior art: [bobi/WsdScanService](https://github.com/bobi/WsdScanService) (CoreWCF). This service stays a thin Rust SOAP layer. GetScannerElements is driven by the active profile.

WSD SOAP is parsed and built through one `wsd::xml::Element` tree (`parse` / `to_xml`); dispatch uses `soap::Request` once per POST, and schema validation walks the same tree.

Published capabilities come from the active JSON profile (dpi lists, formats, page sizes, presence/absence of optical dpi).

## WIA USD path (P1)

A user-mode WIA 2.0 minidriver (patterned on Microsoft `wiadriverex` / `ProdScan`) that:

- Registers under Imaging Devices.
- Reads the same control API / page store (or a shared memory/named-pipe IPC).
- Publishes USB-full property trees for feeder + glass.

Requires INF install (admin once). Prefer unsigned test-signing on lab machines.

## Sticky FEED_READY

Real Brother USB reports feeder “ready” even when empty. Profiles may set:

```json
"feeder": { "sticky_ready": true }
```

so Auto→glass fallback and empty-feeder races can be tested without hardware.

## Out of scope

- Print / fax / copy simulation — use sibling **[fakeprint](../../fakeprint)** (WSD Print first).
- Evading security software or spoofing a vendor’s signed driver.
- Shipping vendor firmware or copyrighted UI strings.

## Multi-device and the device manager

One process hosts N emulated scanners. `manager.rs` (`DeviceManager`) owns the live device set: each
device is its own `World`, prebuilt control/WSD router and WS-Discovery announcer, and can be added or
removed at runtime (`POST /v1/devices`, `DELETE /v1/devices/{id}`) without a restart. HTTP routing is
dynamic: `server::dispatch` resolves `/d/{id}/...` per request (404 JSON if absent) and sends the legacy
`/v1/*`, `/ScannerService`, `/WebServices/Device` paths to the default (first) device, which keeps the
legacy WSD endpoints and XAddr. See `multi-device.md` for the contract.

The manager also owns install tracking: `registry.rs` (`RegistryStore`, atomic locked writes, one row per
device with `install` state and two fingerprints) backs `mark_probed` / `mark_installed` / `reconfigure` /
`reconcile_with_windows`. Daemon runs (`--manage`, `--lab-name`) attach `devices.json` (or `$FAKESCAN_REGISTRY`);
embedded servers keep the same state in memory only. See `lab-instance.md` for the row format and states.
