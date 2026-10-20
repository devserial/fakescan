# MCP server over the control API

fakescan is the device under test. An agent that is writing or running an acquire test needs to load pages, pick a pattern, and arm a fault without a person at the glass. The `/v1` control API already does that. This note is the MCP server in front of it.

The server is the optional crate `crates/fakescan-mcp`, binary `fakescan-mcp`. The scanner binary does not depend on it, and turning the crate off leaves scan behavior unchanged. `docs/lab-instance.md` also mentions a Python `fakescan-mcp` from the separate `fakescan-python` tree. That package is not in this repo. This crate is the in-repo server.

## Attach only

The process does not start, stop, or embed fakescan. `--base-url` (default `http://127.0.0.1:3920`, or `FAKESCAN_BASE_URL`) is the control origin of a scanner that is already listening. `--device` (or `FAKESCAN_DEVICE`) selects `/d/{id}/...` for one device in a multi-device process. The id uses the same `[A-Za-z0-9_-]{1,64}` rule as device ids.

A tool that needs the device and cannot connect returns `device_unreachable`. `server_status` with the `capabilities` section does not dial out, so an agent can still read the tool list when the scanner is down. The `device` section reports `reachable: false` instead of failing the status call.

The base URL may be a LAN address. The scanner is the lab device, including one another machine discovered. The URL must be `http` (the control API is not TLS), with no userinfo and no path. `https` is refused.

`shutdown` and `open_settings` are refused even with `confirm=true`. Stopping the process and launching host settings stay outside this server.

## Names

Tool names are unprefixed and lead with the effect, the same way hardly and the fakeprint MCP do. There is no `fakescan_` prefix.

| Form | Effect |
| --- | --- |
| `send_*` | Changes device state or sends HTTP to the scanner. Returns `{confirmed:false, sent:false, plan, hint}` until `confirm` is the JSON boolean `true`. Nothing is sent on the dry run. |
| `write_*` | Writes a local file. Refuses a path that already exists unless `overwrite` is the JSON boolean `true` (`output_exists`). |
| anything else | Read-only GET. |

`confirm` and `overwrite` accept only a real boolean. The string `"true"`, `1`, and other truthy values are `invalid_argument` and do not send or replace a file.

Names shared with fakeprint, for the same kind of call: `server_status`, `status`, `devices`, `ctl_info`, `jobs`, `send_reset`, `send_faults`, `send_profile`, `send_ctl`. A clear follows fakeprint's `send_requests_clear`: `send_tickets_clear`, `send_loopback_clear`. Scanner-only tools keep that shape (`send_pages`, `send_pattern`, `send_scenario`, `write_log`, and the rest).

`send_ctl` is the confirm-gated form of `POST /v1/ctl` for commands that do not have their own tool (`save`, `installed`, and the rest of the ctl list except `shutdown` and `open_settings`).

`write_log` saves `GET /v1/log` to `output_path`. Writes are allowed only inside the working directory, the OS temp directory, `/workspace` (or `FAKESCAN_WORKSPACE` when that directory exists), or a directory in `FAKESCAN_WRITE_DIRS` (`path_not_allowed`). `FAKESCAN_WRITE_DIRS=*` turns the allowlist off. Home credential directories, shell startup files, `.git`, and special files (devices, sockets, pipes) stay refused either way. A refused path is not fetched.

## Capabilities

`server_status` follows hardly's `hardly_server_status`: `sections` defaults to `capabilities`. The capabilities object is `name`, `version`, `features`, `tools`, `tool_count`, plus `attach_only`. Features include `confirm_gate` and `write_overwrite_guard`. The same list is what `tools/list` advertises, and the `initialize` result carries the same instructions the names describe.

## Transport

Stdio, JSON-RPC, `Content-Length` framing, protocol `2024-11-05`. Tool failures are results with `isError: true` and a JSON body `{error, code, hint}`, not a crashed process.

```json
{
  "fakescan": {
    "command": "fakescan-mcp",
    "args": ["--base-url", "http://127.0.0.1:3920"]
  }
}
```

## Tests

`cargo test -p fakescan-mcp` does not use the network. A scripted client records HTTP calls so a `confirm=false` call can be shown to send nothing. One test binds `127.0.0.1` and checks the HTTP client against that listener. Schema files and a running scanner are not required.
