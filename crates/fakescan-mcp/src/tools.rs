//! Tool surface. Names follow hardly: `send_` changes device state or sends traffic and returns
//! a plan until `confirm=true`; `write_` creates a local file and refuses to replace one unless
//! `overwrite=true`; every other tool only reads.

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::error::{invalid, missing, ToolError};
use crate::guard::prepare_output;
use crate::http::{ClientError, DeviceClient, RawResponse};

pub const INSTRUCTIONS: &str = "\
fakescan-mcp drives a fake WSD scanner that is already running. It attaches to the \
/v1 control API and never starts or stops fakescan. Use it to arm pages, faults, \
patterns, and scenarios before an acquire test.\n\
\n\
Names tell the effect: send_* changes device state or sends traffic and \
returns a plan until confirm=true; write_* writes a local file and refuses \
to replace one unless overwrite=true; every other tool only reads.\n\
\n\
Start with server_status. If the device section says reachable is false, \
start fakescan yourself and fix --base-url (FAKESCAN_BASE_URL). Optional device id \
selects /d/{id}/... and uses the same [A-Za-z0-9_-]{1,64} rule as the scanner.\n\
\n\
send_* tools send nothing without confirm=true. Tell the person what will change, \
then repeat the call with confirm=true. shutdown and open_settings are refused: \
this server does not stop the process or launch host settings.\n\
\n\
write_* tools write only inside the working directory, the temp directory, \
/workspace, or FAKESCAN_WRITE_DIRS. A path that already exists needs overwrite=true.\
";

const CONFIRM_HINT: &str = "Nothing was sent. Tell the person exactly what will be sent, then repeat the same call with confirm=true.";

/// Commands `POST /v1/ctl` accepts. Kept in step with `ctl::CTL_CMDS`.
const CTL_CMDS: &[&str] = &[
    "status",
    "health",
    "jobs",
    "activity",
    "tickets",
    "profile",
    "pattern",
    "pages",
    "feeder",
    "jpeg_com",
    "effects",
    "scenario",
    "fault",
    "fault_plan",
    "slow",
    "loopback",
    "push_scan",
    "reset",
    "save",
    "installed",
    "mark_added",
    "forget",
    "reconcile",
    "shutdown",
    "open_settings",
];

const FEATURES: &[&str] = &[
    "confirm_gate",
    "write_overwrite_guard",
    "attach_only",
    "server_status",
    "device_prefix",
    "ctl",
];

pub fn capabilities() -> Value {
    let tools = tool_names();
    json!({
        "name": "fakescan",
        "version": env!("CARGO_PKG_VERSION"),
        "features": FEATURES,
        "tools": tools,
        "tool_count": tools.len(),
        "attach_only": true,
        "next": "Call server_status. send_* returns a plan until confirm=true. write_* refuses an existing file unless overwrite=true. This server does not start or stop fakescan.",
    })
}

fn tool_names() -> Vec<&'static str> {
    catalog().into_iter().map(|t| t.name).collect()
}

struct Tool {
    name: &'static str,
    description: &'static str,
    schema: Value,
}

pub fn tool_list() -> Vec<Value> {
    catalog()
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.schema,
            })
        })
        .collect()
}

fn catalog() -> Vec<Tool> {
    let device = json!({"type": "string", "description": "Device id. Selects /d/{id}/. Omit for the default device."});
    let confirm = json!({"type": "boolean", "description": "Send only when this is the JSON boolean true. The string \"true\" is rejected. Otherwise the tool returns a plan.", "default": false});
    let overwrite = json!({"type": "boolean", "description": "Replace an existing file only when this is the JSON boolean true. The string \"true\" is rejected.", "default": false});
    let output_path = json!({"type": "string", "description": "File to write. Must sit in the working directory, temp, /workspace, or FAKESCAN_WRITE_DIRS."});
    vec![
        Tool {
            name: "server_status",
            description: "Server version, feature flags, and tool names. sections: capabilities (default), device. capabilities does not contact the scanner. device reports reachable true or false. Example: server_status(sections=['capabilities']).",
            schema: object(
                &[("sections", json!({"type": "array", "items": {"type": "string"}})), ("device", device.clone())],
                &[],
            ),
        },
        read("ctl_info", "GET /v1/ctl. Commands this device accepts, plus port, uuid, and base URL. Read-only.", "/v1/ctl", &device),
        read("status", "GET /v1/status. Feeder, pages, faults, and the active profile. Read-only.", "/v1/status", &device),
        read("jobs", "GET /v1/jobs. Active and recent scan jobs. Read-only.", "/v1/jobs", &device),
        read("activity", "GET /v1/activity. Recent control and WSD calls. Optional after is a numeric cursor. Read-only.", "/v1/activity", &device),
        read("tickets", "GET /v1/tickets. Scan tickets the device has seen. Read-only.", "/v1/tickets", &device),
        read("scenarios", "GET /v1/scenarios. Named lab progressions. Read-only.", "/v1/scenarios", &device),
        read("devices", "GET /v1/devices. Devices this process is hosting. Read-only. Not scoped under /d/{id}.", "/v1/devices", &device),
        read("profiles", "GET /v1/profiles. Profile ids the process can load. Read-only.", "/v1/profiles", &device),
        read("loopback", "GET /v1/loopback. Whether hard loopback pages are armed. Read-only.", "/v1/loopback", &device),
        read("fault_plan", "GET /v1/faults/plan. The scripted fault plan, if any. Read-only.", "/v1/faults/plan", &device),
        Tool {
            name: "send_pages",
            description: "POST /v1/pages. Loads the feeder with count sheets. confirm=false returns the plan and sends nothing. Example: send_pages(count=2, confirm=true).",
            schema: object(&[("count", json!({"type": "integer", "minimum": 0})), ("side", json!({"type": "string"})), ("order", json!({"type": "string"})), ("confirm", confirm.clone()), ("device", device.clone())], &["count"]),
        },
        Tool {
            name: "send_pattern",
            description: "POST /v1/pattern. Selects the synthetic page layout by id. confirm-gated.",
            schema: object(&[("id", json!({"type": "string"})), ("confirm", confirm.clone()), ("device", device.clone())], &["id"]),
        },
        Tool {
            name: "send_effects",
            description: "POST /v1/effects. Applies an effects object (the same JSON the control API accepts). confirm-gated.",
            schema: object(&[("effects", json!({"type": "object"})), ("confirm", confirm.clone()), ("device", device.clone())], &["effects"]),
        },
        Tool {
            name: "send_scenario",
            description: "POST /v1/scenario. Applies one named progression step. confirm-gated.",
            schema: object(&[("id", json!({"type": "string"})), ("confirm", confirm.clone()), ("device", device.clone())], &["id"]),
        },
        Tool {
            name: "send_faults",
            description: "POST /v1/fault. Sets jam, cover, offline, and the other fault flags. Pass the flags as fault={...}. confirm-gated. An empty fault object is a no-op on the device.",
            schema: object(&[("fault", json!({"type": "object"})), ("confirm", confirm.clone()), ("device", device.clone())], &["fault"]),
        },
        Tool {
            name: "send_feeder",
            description: "POST /v1/feeder. Sets whether the feeder reports ready. confirm-gated.",
            schema: object(&[("ready", json!({"type": "boolean"})), ("confirm", confirm.clone()), ("device", device.clone())], &["ready"]),
        },
        Tool {
            name: "send_profile",
            description: "POST /v1/profile. Loads a profile by id ([A-Za-z0-9_-], 1-64). confirm-gated.",
            schema: object(&[("id", json!({"type": "string"})), ("confirm", confirm.clone()), ("device", device.clone())], &["id"]),
        },
        Tool {
            name: "send_slow",
            description: "POST /v1/slow. Sets the retrieve delay (ms) and/or byte rate (bps). confirm-gated. At least one of ms or bps is required.",
            schema: object(&[("ms", json!({"type": "integer", "minimum": 0})), ("bps", json!({"type": "integer", "minimum": 0})), ("confirm", confirm.clone()), ("device", device.clone())], &[]),
        },
        Tool {
            name: "send_jpeg_com",
            description: "POST /v1/jpeg-com. Turns JFIF comment markers on or off. confirm-gated.",
            schema: object(&[("enabled", json!({"type": "boolean"})), ("confirm", confirm.clone()), ("device", device.clone())], &["enabled"]),
        },
        Tool {
            name: "send_loopback",
            description: "POST /v1/loopback. Arms hard loopback from base64 page blobs. confirm-gated. The plan reports page count and sizes, not the blobs.",
            schema: object(&[("pages", json!({"type": "array", "items": {"type": "string"}})), ("confirm", confirm.clone()), ("device", device.clone())], &["pages"]),
        },
        Tool {
            name: "send_loopback_clear",
            description: "DELETE /v1/loopback. Drops hard loopback pages. confirm-gated.",
            schema: object(&[("confirm", confirm.clone()), ("device", device.clone())], &[]),
        },
        Tool {
            name: "send_reset",
            description: "POST /v1/reset. Returns the device to the profile defaults. confirm-gated.",
            schema: object(&[("confirm", confirm.clone()), ("device", device.clone())], &[]),
        },
        Tool {
            name: "send_push_scan",
            description: "POST /v1/ctl cmd push_scan. Mints a ScanIdentifier and, when a NotifyTo sink is stored, posts ScanAvailableEvent. confirm-gated.",
            schema: object(&[("destination_token", json!({"type": "string"})), ("confirm", confirm.clone()), ("device", device.clone())], &[]),
        },
        Tool {
            name: "send_tickets_clear",
            description: "DELETE /v1/tickets. Clears the ticket log. confirm-gated.",
            schema: object(&[("confirm", confirm.clone()), ("device", device.clone())], &[]),
        },
        Tool {
            name: "send_ctl",
            description: "POST /v1/ctl for a named command. confirm-gated. shutdown and open_settings are refused. Prefer the specific send_* tool when one exists.",
            schema: object(&[("cmd", json!({"type": "string"})), ("args", json!({"type": "object"})), ("confirm", confirm.clone()), ("device", device.clone())], &["cmd"]),
        },
        Tool {
            name: "write_log",
            description: "Writes a file: GET /v1/log saved to output_path. Refuses to replace an existing file unless overwrite=true. Writes only inside the working directory, temp, /workspace, or FAKESCAN_WRITE_DIRS.",
            schema: object(&[("output_path", output_path), ("overwrite", overwrite), ("after", json!({"type": "string", "description": "Numeric cursor. Omit for the whole log."})), ("device", device.clone())], &["output_path"]),
        },
    ]
}

fn read(name: &'static str, description: &'static str, _path: &str, device: &Value) -> Tool {
    let mut schema_props_device = device.clone();
    if name == "activity" {
        schema_props_device = device.clone();
    }
    let schema = if name == "activity" {
        object(
            &[
                ("after", json!({"type": "string"})),
                ("device", schema_props_device),
            ],
            &[],
        )
    } else {
        object(&[("device", device.clone())], &[])
    };
    Tool {
        name,
        description,
        schema,
    }
}

fn object(props: &[(&str, Value)], required: &[&str]) -> Value {
    let mut properties = Map::new();
    for (key, value) in props {
        properties.insert((*key).to_string(), value.clone());
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub struct Session<C> {
    pub(crate) client: C,
    device: Option<String>,
}

impl<C: DeviceClient> Session<C> {
    pub fn new(client: C, device: Option<String>) -> Result<Self, String> {
        if let Some(id) = &device {
            if !valid_id(id) {
                return Err(format!(
                    "device id {id:?} must match [A-Za-z0-9_-] and be 1 to 64 characters"
                ));
            }
        }
        Ok(Self { client, device })
    }

    pub fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, ToolError> {
        if !tool_names().contains(&name) {
            return Err(ToolError::new(
                format!("unknown tool {name}"),
                "unknown_tool",
                Some("Call server_status for the tool list.".into()),
            ));
        }
        let args = arguments.as_object().cloned().unwrap_or_default();
        match name {
            "server_status" => self.server_status(&args),
            "ctl_info" => self.get_json(&args, "/v1/ctl", true),
            "status" => self.get_json(&args, "/v1/status", true),
            "jobs" => self.get_json(&args, "/v1/jobs", true),
            "activity" => self.activity(&args),
            "tickets" => self.get_json(&args, "/v1/tickets", true),
            "scenarios" => self.get_json(&args, "/v1/scenarios", true),
            "devices" => self.get_json(&args, "/v1/devices", false),
            "profiles" => self.get_json(&args, "/v1/profiles", false),
            "loopback" => self.get_json(&args, "/v1/loopback", true),
            "fault_plan" => self.get_json(&args, "/v1/faults/plan", true),
            "send_pages" => self.send_pages(&args),
            "send_pattern" => self.send_id(&args, "send_pattern", "/v1/pattern"),
            "send_effects" => self.send_object(&args, "send_effects", "/v1/effects", "effects"),
            "send_scenario" => self.send_id(&args, "send_scenario", "/v1/scenario"),
            "send_faults" => self.send_object(&args, "send_faults", "/v1/fault", "fault"),
            "send_feeder" => self.send_feeder(&args),
            "send_profile" => self.send_profile(&args),
            "send_slow" => self.send_slow(&args),
            "send_jpeg_com" => self.send_jpeg_com(&args),
            "send_loopback" => self.send_loopback(&args),
            "send_loopback_clear" => self.send_no_body(&args, "DELETE", "/v1/loopback"),
            "send_reset" => self.send_no_body(&args, "POST", "/v1/reset"),
            "send_push_scan" => self.send_push_scan(&args),
            "send_tickets_clear" => self.send_no_body(&args, "DELETE", "/v1/tickets"),
            "send_ctl" => self.send_ctl(&args),
            "write_log" => self.write_log(&args),
            _ => Err(ToolError::new(
                format!("unknown tool {name}"),
                "unknown_tool",
                None,
            )),
        }
    }

    fn device_of(
        &self,
        args: &Map<String, Value>,
        allowed: &[&str],
    ) -> Result<Option<String>, ToolError> {
        expect_keys(args, allowed)?;
        match args.get("device") {
            None | Some(Value::Null) => Ok(self.device.clone()),
            Some(Value::String(id)) if valid_id(id) => Ok(Some(id.clone())),
            Some(other) => Err(invalid(
                "device",
                format!("{other} must match [A-Za-z0-9_-] and be 1 to 64 characters"),
            )),
        }
    }

    fn scoped(&self, device: Option<&str>, path: &str, per_device: bool) -> String {
        if per_device {
            if let Some(id) = device {
                return format!("/d/{id}{path}");
            }
        }
        path.to_string()
    }

    fn server_status(&mut self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["sections", "device"])?;
        let sections = match args.get("sections") {
            None | Some(Value::Null) => vec!["capabilities".to_string()],
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| invalid("server_status", "sections must be strings"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err(invalid("server_status", "sections must be an array"));
            }
        };
        let sections = if sections.is_empty() {
            vec!["capabilities".to_string()]
        } else {
            sections
        };
        for section in &sections {
            if section != "capabilities" && section != "device" {
                return Err(ToolError::new(
                    format!("server_status: unknown section(s): {section}"),
                    "unknown_section",
                    Some("Valid sections: capabilities, device. Default: capabilities.".into()),
                ));
            }
        }
        let mut out = json!({ "sections": sections });
        if sections.iter().any(|s| s == "capabilities") {
            out["capabilities"] = capabilities();
        }
        if sections.iter().any(|s| s == "device") {
            let path = self.scoped(device.as_deref(), "/health", false);
            match self.client.exchange("GET", &path, None) {
                Ok(raw) if (200..300).contains(&raw.status) => {
                    out["device"] = json!({"reachable": true, "status": raw.status});
                }
                Ok(raw) => {
                    out["device"] = json!({
                        "reachable": false,
                        "status": raw.status,
                        "error": body_text(&raw),
                    });
                }
                Err(ClientError::Unreachable(message)) => {
                    out["device"] = json!({"reachable": false, "error": message});
                }
                Err(ClientError::Protocol(message)) => {
                    out["device"] = json!({"reachable": false, "error": message});
                }
            }
        }
        Ok(out)
    }

    fn activity(&mut self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["after", "device"])?;
        let mut path = self.scoped(device.as_deref(), "/v1/activity", true);
        if let Some(after) = optional_string(args, "after")? {
            if after.is_empty() || !after.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid("activity", "after must be a decimal cursor"));
            }
            path.push_str("?after=");
            path.push_str(&after);
        }
        self.fetch("GET", &path, None)
    }

    fn get_json(
        &mut self,
        args: &Map<String, Value>,
        path: &str,
        per_device: bool,
    ) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["device"])?;
        let path = self.scoped(device.as_deref(), path, per_device);
        self.fetch("GET", &path, None)
    }

    fn send_pages(&mut self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["count", "side", "order", "confirm", "device"])?;
        let count = required_u32(args, "send_pages", "count")?;
        let mut body = json!({"count": count});
        if let Some(side) = optional_string(args, "side")? {
            body["side"] = json!(side);
        }
        if let Some(order) = optional_string(args, "order")? {
            body["order"] = json!(order);
        }
        let path = self.scoped(device.as_deref(), "/v1/pages", true);
        self.confirm_then(args, "POST", &path, body)
    }

    fn send_id(
        &mut self,
        args: &Map<String, Value>,
        tool: &str,
        path: &str,
    ) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["id", "confirm", "device"])?;
        let id = required_string(args, tool, "id")?;
        let path = self.scoped(device.as_deref(), path, true);
        self.confirm_then(args, "POST", &path, json!({"id": id}))
    }

    fn send_object(
        &mut self,
        args: &Map<String, Value>,
        tool: &str,
        path: &str,
        field: &str,
    ) -> Result<Value, ToolError> {
        let device = self.device_of(args, &[field, "confirm", "device"])?;
        let body = match args.get(field) {
            Some(Value::Object(map)) => Value::Object(map.clone()),
            Some(_) => return Err(invalid(tool, format!("{field} must be an object"))),
            None => return Err(missing(tool, field)),
        };
        let path = self.scoped(device.as_deref(), path, true);
        self.confirm_then(args, "POST", &path, body)
    }

    fn send_feeder(&mut self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["ready", "confirm", "device"])?;
        let ready = required_bool(args, "send_feeder", "ready")?;
        let path = self.scoped(device.as_deref(), "/v1/feeder", true);
        self.confirm_then(args, "POST", &path, json!({"ready": ready}))
    }

    fn send_profile(&mut self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["id", "confirm", "device"])?;
        let id = required_string(args, "send_profile", "id")?;
        if !valid_id(&id) {
            return Err(invalid(
                "send_profile",
                "id must match [A-Za-z0-9_-] and be 1 to 64 characters",
            ));
        }
        let path = self.scoped(device.as_deref(), "/v1/profile", true);
        self.confirm_then(args, "POST", &path, json!({"id": id}))
    }

    fn send_slow(&mut self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["ms", "bps", "confirm", "device"])?;
        if args.get("ms").is_none() && args.get("bps").is_none() {
            return Err(missing("send_slow", "ms or bps"));
        }
        let mut body = Map::new();
        if let Some(ms) = optional_u64(args, "send_slow", "ms")? {
            body.insert("ms".into(), json!(ms));
        }
        if let Some(bps) = optional_u64(args, "send_slow", "bps")? {
            body.insert("bps".into(), json!(bps));
        }
        let path = self.scoped(device.as_deref(), "/v1/slow", true);
        self.confirm_then(args, "POST", &path, Value::Object(body))
    }

    fn send_jpeg_com(&mut self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["enabled", "confirm", "device"])?;
        let enabled = required_bool(args, "send_jpeg_com", "enabled")?;
        let path = self.scoped(device.as_deref(), "/v1/jpeg-com", true);
        self.confirm_then(args, "POST", &path, json!({"enabled": enabled}))
    }

    fn send_loopback(&mut self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["pages", "confirm", "device"])?;
        let pages = match args.get("pages") {
            Some(Value::Array(items)) => items,
            Some(_) => {
                return Err(invalid(
                    "send_loopback",
                    "pages must be an array of strings",
                ))
            }
            None => return Err(missing("send_loopback", "pages")),
        };
        if pages.len() > 64 {
            return Err(invalid("send_loopback", "pages is limited to 64 blobs"));
        }
        let mut sizes = Vec::new();
        for page in pages {
            let Some(text) = page.as_str() else {
                return Err(invalid(
                    "send_loopback",
                    "each page must be a base64 string",
                ));
            };
            if text.len() > 8 * 1024 * 1024 {
                return Err(invalid("send_loopback", "a page blob exceeds 8 MiB"));
            }
            sizes.push(text.len());
        }
        let path = self.scoped(device.as_deref(), "/v1/loopback", true);
        let confirm = flag(args, "confirm")?;
        let plan = json!({
            "method": "POST",
            "path": path,
            "pages": pages.len(),
            "bytes": sizes,
        });
        if !confirm {
            return Ok(planned(plan));
        }
        self.fetch("POST", &path, Some(json!({"pages": pages})))
    }

    fn send_push_scan(&mut self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["destination_token", "confirm", "device"])?;
        let mut ctl_args = Map::new();
        if let Some(token) = optional_string(args, "destination_token")? {
            if token.chars().any(|c| c.is_control()) || token.len() > 128 {
                return Err(invalid(
                    "send_push_scan",
                    "destination_token contains controls or is longer than 128 characters",
                ));
            }
            ctl_args.insert("destination_token".into(), json!(token));
        }
        let path = self.scoped(device.as_deref(), "/v1/ctl", true);
        self.confirm_then(
            args,
            "POST",
            &path,
            json!({"cmd": "push_scan", "args": ctl_args}),
        )
    }

    fn send_no_body(
        &mut self,
        args: &Map<String, Value>,
        method: &str,
        path: &str,
    ) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["confirm", "device"])?;
        let path = self.scoped(device.as_deref(), path, true);
        let confirm = flag(args, "confirm")?;
        let plan = json!({"method": method, "path": path});
        if !confirm {
            return Ok(planned(plan));
        }
        let body = if method == "POST" {
            Some(json!({}))
        } else {
            None
        };
        self.fetch(method, &path, body)
    }

    fn send_ctl(&mut self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["cmd", "args", "confirm", "device"])?;
        let cmd = required_string(args, "send_ctl", "cmd")?;
        if !CTL_CMDS.contains(&cmd.as_str()) {
            return Err(ToolError::new(
                format!("send_ctl: unknown cmd {cmd:?}"),
                "unknown_cmd",
                Some(format!("Valid cmd values: {}.", CTL_CMDS.join(", "))),
            ));
        }
        if cmd == "shutdown" || cmd == "open_settings" {
            return Err(ToolError::new(
                format!("send_ctl: {cmd} is not available"),
                "not_supported",
                Some(
                    "This server does not stop fakescan or launch host settings. Stop the process yourself."
                        .into(),
                ),
            ));
        }
        let ctl_args = match args.get("args") {
            None | Some(Value::Null) => json!({}),
            Some(Value::Object(map)) => Value::Object(map.clone()),
            Some(_) => return Err(invalid("send_ctl", "args must be an object")),
        };
        let path = self.scoped(device.as_deref(), "/v1/ctl", true);
        self.confirm_then(args, "POST", &path, json!({"cmd": cmd, "args": ctl_args}))
    }

    fn write_log(&mut self, args: &Map<String, Value>) -> Result<Value, ToolError> {
        let device = self.device_of(args, &["output_path", "overwrite", "after", "device"])?;
        let output = required_string(args, "write_log", "output_path")?;
        let overwrite = flag(args, "overwrite")?;
        let path = Path::new(&output);
        prepare_output(path, overwrite)?;
        let mut api = self.scoped(device.as_deref(), "/v1/log", true);
        if let Some(after) = optional_string(args, "after")? {
            if after.is_empty() || !after.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid("write_log", "after must be a decimal cursor"));
            }
            api.push_str("?after=");
            api.push_str(&after);
        }
        let raw = self
            .client
            .exchange("GET", &api, None)
            .map_err(ClientError::into_tool)?;
        if !(200..300).contains(&raw.status) {
            return Err(rejected(&raw));
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    ToolError::new(
                        format!("cannot create {}: {e}", parent.display()),
                        "invalid_argument",
                        None,
                    )
                })?;
            }
        }
        std::fs::write(path, &raw.body).map_err(|e| {
            ToolError::new(
                format!("cannot write {}: {e}", path.display()),
                "invalid_argument",
                None,
            )
        })?;
        Ok(json!({
            "output_path": output,
            "bytes": raw.body.len(),
            "overwrite": overwrite,
        }))
    }

    fn confirm_then(
        &mut self,
        args: &Map<String, Value>,
        method: &str,
        path: &str,
        body: Value,
    ) -> Result<Value, ToolError> {
        let confirm = flag(args, "confirm")?;
        if !confirm {
            return Ok(planned(json!({
                "method": method,
                "path": path,
                "body": body,
            })));
        }
        self.fetch(method, path, Some(body))
    }

    fn fetch(&mut self, method: &str, path: &str, body: Option<Value>) -> Result<Value, ToolError> {
        let encoded = match &body {
            Some(value) => Some(
                serde_json::to_vec(value)
                    .map_err(|e| ToolError::new(format!("json: {e}"), "invalid_argument", None))?,
            ),
            None => None,
        };
        let raw = self
            .client
            .exchange(method, path, encoded.as_deref())
            .map_err(ClientError::into_tool)?;
        if !(200..300).contains(&raw.status) {
            return Err(rejected(&raw));
        }
        if raw.body.is_empty() {
            return Ok(json!({}));
        }
        match serde_json::from_slice(&raw.body) {
            Ok(value) => Ok(value),
            Err(_) => Ok(json!({"text": body_text(&raw)})),
        }
    }
}

fn planned(plan: Value) -> Value {
    json!({
        "confirmed": false,
        "sent": false,
        "plan": plan,
        "hint": CONFIRM_HINT,
    })
}

fn rejected(raw: &RawResponse) -> ToolError {
    let text = body_text(raw);
    let clipped: String = text.chars().take(500).collect();
    ToolError::new(
        format!("device returned HTTP {}: {clipped}", raw.status),
        "device_rejected",
        None,
    )
}

fn body_text(raw: &RawResponse) -> String {
    String::from_utf8_lossy(&raw.body).into_owned()
}

fn expect_keys(args: &Map<String, Value>, allowed: &[&str]) -> Result<(), ToolError> {
    let unknown: Vec<_> = args
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .cloned()
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(invalid(
            "arguments",
            format!("unknown field(s): {}", unknown.join(", ")),
        ))
    }
}

/// `confirm` and `overwrite` are JSON booleans. A string `"true"`, `1`, or any other
/// truthy value is rejected so a quoted flag cannot send traffic or replace a file.
fn flag(args: &Map<String, Value>, name: &str) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ToolError::new(
            format!("{name} must be a boolean"),
            "invalid_argument",
            Some("Use true or false. A string such as \"true\" is not accepted.".into()),
        )),
    }
}

fn required_string(args: &Map<String, Value>, tool: &str, name: &str) -> Result<String, ToolError> {
    match args.get(name) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        Some(Value::String(_)) | None | Some(Value::Null) => Err(missing(tool, name)),
        Some(_) => Err(invalid(tool, format!("{name} must be a string"))),
    }
}

fn optional_string(args: &Map<String, Value>, name: &str) -> Result<Option<String>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid(name, format!("{name} must be a string"))),
    }
}

fn required_bool(args: &Map<String, Value>, tool: &str, name: &str) -> Result<bool, ToolError> {
    match args.get(name) {
        Some(Value::Bool(value)) => Ok(*value),
        None | Some(Value::Null) => Err(missing(tool, name)),
        Some(_) => Err(invalid(tool, format!("{name} must be a boolean"))),
    }
}

fn required_u32(args: &Map<String, Value>, tool: &str, name: &str) -> Result<u32, ToolError> {
    match args.get(name) {
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| {
                invalid(
                    tool,
                    format!("{name} must be an integer from 0 to 4294967295"),
                )
            }),
        None | Some(Value::Null) => Err(missing(tool, name)),
        Some(_) => Err(invalid(tool, format!("{name} must be an integer"))),
    }
}

fn optional_u64(
    args: &Map<String, Value>,
    tool: &str,
    name: &str,
) -> Result<Option<u64>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid(tool, format!("{name} must be a non-negative integer"))),
        Some(_) => Err(invalid(tool, format!("{name} must be an integer"))),
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ScriptedClient;
    use serde_json::json;

    fn session() -> Session<ScriptedClient> {
        Session::new(ScriptedClient::default(), None).unwrap()
    }

    #[test]
    fn capabilities_lists_every_tool_and_the_gates() {
        let caps = capabilities();
        let names = tool_names();
        assert_eq!(caps["tool_count"], names.len());
        assert_eq!(caps["tools"].as_array().unwrap().len(), names.len());
        assert_eq!(caps["name"], "fakescan");
        assert!(caps["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "confirm_gate"));
        assert!(caps["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "write_overwrite_guard"));
        assert!(names.contains(&"server_status"));
        assert!(names.contains(&"send_faults"));
        assert!(names.contains(&"send_tickets_clear"));
        assert!(names.contains(&"write_log"));
        for name in &names {
            assert!(!name.starts_with("fakescan_"), "{name}");
            assert!(!name.starts_with("hardly_"), "{name}");
        }
        let listed: Vec<String> = tool_list()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(listed.len(), names.len());
    }

    #[test]
    fn send_pages_without_confirm_does_not_call_the_device() {
        let mut s = session();
        let out = s.call("send_pages", &json!({"count": 2})).unwrap();
        assert_eq!(out["confirmed"], false);
        assert_eq!(out["sent"], false);
        assert_eq!(out["plan"]["method"], "POST");
        assert_eq!(out["plan"]["path"], "/v1/pages");
        assert_eq!(out["plan"]["body"]["count"], 2);
        assert!(out["hint"].as_str().unwrap().contains("confirm=true"));
        assert!(s.client.calls.is_empty());
    }

    #[test]
    fn send_pages_with_confirm_posts_once() {
        let mut s = session();
        s.client.body = br#"{"pages_remaining":2}"#.to_vec();
        let out = s
            .call("send_pages", &json!({"count": 2, "confirm": true}))
            .unwrap();
        assert_eq!(out["pages_remaining"], 2);
        assert_eq!(s.client.calls.len(), 1);
        assert_eq!(s.client.calls[0].method, "POST");
        assert_eq!(s.client.calls[0].path, "/v1/pages");
        assert_eq!(
            s.client.calls[0].body.as_deref(),
            Some(br#"{"count":2}"#.as_slice())
        );
    }

    #[test]
    fn device_id_is_prefixed_and_a_bad_id_is_rejected_before_any_call() {
        let mut s = Session::new(ScriptedClient::default(), Some("lab1".into())).unwrap();
        s.client.body = b"{}".to_vec();
        s.call("status", &json!({})).unwrap();
        assert_eq!(s.client.calls[0].path, "/d/lab1/v1/status");
        let err = s
            .call("status", &json!({"device": "../other"}))
            .unwrap_err();
        assert_eq!(err.code, "invalid_argument");
        assert_eq!(s.client.calls.len(), 1);
    }

    #[test]
    fn shutdown_is_refused_even_when_confirmed() {
        let mut s = session();
        let err = s
            .call("send_ctl", &json!({"cmd": "shutdown", "confirm": true}))
            .unwrap_err();
        assert_eq!(err.code, "not_supported");
        assert!(s.client.calls.is_empty());
    }

    #[test]
    fn unreachable_device_is_an_error_and_status_still_reports_capabilities() {
        let mut s = session();
        s.client.fail = Some("connection refused".into());
        let err = s.call("status", &json!({})).unwrap_err();
        assert_eq!(err.code, "device_unreachable");
        let status = s
            .call(
                "server_status",
                &json!({"sections": ["capabilities", "device"]}),
            )
            .unwrap();
        assert_eq!(status["capabilities"]["name"], "fakescan");
        assert_eq!(status["device"]["reachable"], false);
    }

    #[test]
    fn write_log_refuses_overwrite_and_a_path_outside_the_allowlist() {
        let mut s = session();
        s.client.body = b"line\n".to_vec();
        let err = s
            .call(
                "write_log",
                &json!({"output_path": "/no-such-fakescan-write/log.txt"}),
            )
            .unwrap_err();
        assert_eq!(err.code, "path_not_allowed");
        assert!(s.client.calls.is_empty(), "a refused path is not fetched");

        let dir = std::env::temp_dir().join(format!("fakescan-mcp-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("log.txt");
        std::fs::write(&file, b"old").unwrap();
        let err = s
            .call("write_log", &json!({"output_path": file.to_str().unwrap()}))
            .unwrap_err();
        assert_eq!(err.code, "output_exists");
        assert_eq!(std::fs::read(&file).unwrap(), b"old");
        assert!(s.client.calls.is_empty());

        let fresh = dir.join("fresh.txt");
        let out = s
            .call(
                "write_log",
                &json!({"output_path": fresh.to_str().unwrap()}),
            )
            .unwrap();
        assert_eq!(out["bytes"], 5);
        assert_eq!(std::fs::read(&fresh).unwrap(), b"line\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirm_and_overwrite_reject_a_truthy_string() {
        let mut s = session();
        for confirm in [json!("true"), json!(1), json!("yes")] {
            let err = s
                .call("send_pages", &json!({"count": 2, "confirm": confirm}))
                .unwrap_err();
            assert_eq!(err.code, "invalid_argument");
            assert!(err.hint.unwrap().contains("true or false"));
        }
        assert!(s.client.calls.is_empty());

        let dir = std::env::temp_dir().join(format!("fakescan-mcp-bool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("log.txt");
        std::fs::write(&file, b"old").unwrap();
        let err = s
            .call(
                "write_log",
                &json!({"output_path": file.to_str().unwrap(), "overwrite": "true"}),
            )
            .unwrap_err();
        assert_eq!(err.code, "invalid_argument");
        assert_eq!(std::fs::read(&file).unwrap(), b"old");
        assert!(s.client.calls.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn server_status_capabilities_does_not_touch_the_client() {
        let mut s = session();
        s.client.fail = Some("down".into());
        let out = s.call("server_status", &json!({})).unwrap();
        assert!(out["capabilities"]["tools"].is_array());
        assert!(s.client.calls.is_empty());
    }
}
