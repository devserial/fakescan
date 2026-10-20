# fakescan docs

Root [README](../README.md) is the quick start.

| Doc | What it is |
|-----|------------|
| [siblings.md](siblings.md) | **vs fakeprint** — ports, ground truth, what not to copy |
| [runbook-fake-scanner.md](runbook-fake-scanner.md) | **Operator checklist** — build → Advertise → Add → WIA acquire by name |
| [lab-harness.md](lab-harness.md) | **Python client** + hard loopback (optional; WIA/lab); read-only `GET /v1/jobs`, `/v1/activity`, `/v1/log` |
| [deploy.md](deploy.md) | Docker / GHCR / LAN advertise |
| [windows.md](windows.md) | PowerShell helpers, discovery verdicts, elevated helper for Imaging remove, Add device (person required) |
| [linux-client.md](linux-client.md) | sane-airscan / `scanimage` against `/ScannerService` |
| [patterns.md](patterns.md) | Synthetic page layouts, QR, tester progression |
| [diag-page-v3.md](diag-page-v3.md) | The default `diag` test page: parametric layout v3, any sheet size, layout JSON + vector primitives |
| [defects.md](defects.md) | **Shipped** image effects, presets, Gray8 |
| [formats.md](formats.md) | WSD/WIA image formats (full catalog except jbig) |
| [reference/](reference/README.md) | **Official WSD/WS-Scan distillate** — Format fallback, MustHonor, Validate vs Create, faults; checked-in XSD/WSDL |
| [coverage.md](coverage.md) | Spec → fakescan completion map (drives roadmap from schema gaps) |
| [roadmap.md](roadmap.md) | Full idea catalog (shipped / next / later) |
| [test-matrix.md](test-matrix.md) | pdfcraft behaviors vs fakescan knobs |
| [architecture.md](architecture.md) | Components + control API |
| [proposals/mcp.md](proposals/mcp.md) | Optional MCP server over `/v1` (attach-only, confirm gate) |
| [specs/](specs/README.md) | Stack notes — DPWS, Discovery, Transfer, Eventing, Windows client (narrative; prefer `reference/` for schema rules) |
| [e2e-wia-status.md](e2e-wia-status.md) | Dated **lab log** (checklist: runbook; scripts: windows.md) |
| [prior-art.md](prior-art.md) | Why this repo exists vs WIA samples / WsdScanService |
| [../CONTRIBUTING.md](../CONTRIBUTING.md) | fmt, clippy, tests, license |
| [assets/](assets/) | Logo: scanner art + two-letter monogram **Fs** |

Print lab lives in the sibling repo — [fakeprint docs](../../fakeprint/docs/README.md) / [siblings.md](siblings.md).
