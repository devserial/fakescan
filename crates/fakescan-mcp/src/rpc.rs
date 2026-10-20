//! Stdio JSON-RPC for MCP. Messages are LSP-style `Content-Length` frames.
//! A tool failure is a normal result with `isError: true`, so the model sees the code and hint.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::http::DeviceClient;
use crate::tools::{tool_list, Session, INSTRUCTIONS};

pub fn serve<C, R, W>(session: &mut Session<C>, reader: R, mut writer: W) -> std::io::Result<()>
where
    C: DeviceClient,
    R: BufRead,
    W: Write,
{
    let mut reader = reader;
    while let Some(body) = read_message(&mut reader)? {
        let message: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => {
                write_message(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": {"code": -32700, "message": "parse error"},
                    }),
                )?;
                continue;
            }
        };
        if let Some(response) = handle(session, &message) {
            write_message(&mut writer, &response)?;
        }
    }
    Ok(())
}

pub fn handle<C: DeviceClient>(session: &mut Session<C>, message: &Value) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let id = message.get("id").cloned()?;
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "fakescan", "version": env!("CARGO_PKG_VERSION")},
            "instructions": INSTRUCTIONS,
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tool_list()}),
        "tools/call" => tool_call(session, message.get("params").unwrap_or(&Value::Null)),
        "" => {
            return Some(error(&id, -32600, "invalid request"));
        }
        _ => return Some(error(&id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn tool_call<C: DeviceClient>(session: &mut Session<C>, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !arguments.is_object() && !arguments.is_null() {
        return tool_error(crate::error::ToolError::new(
            "arguments must be an object",
            "invalid_argument",
            None,
        ));
    }
    match session.call(name, &arguments) {
        Ok(value) => json!({
            "content": [{"type": "text", "text": pretty(&value)}],
            "isError": false,
        }),
        Err(err) => tool_error(err),
    }
}

fn tool_error(err: crate::error::ToolError) -> Value {
    json!({
        "content": [{"type": "text", "text": pretty(&err.json())}],
        "isError": true,
    })
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn error(id: &Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}

pub fn read_message<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut header = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            if header.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated mcp header",
            ));
        }
        header.push_str(&line);
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut length = None;
    for line in header.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            length = rest.trim().parse::<usize>().ok();
        }
    }
    let Some(length) = length else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "mcp frame is missing Content-Length",
        ));
    };
    if length > 16 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "mcp frame exceeds 16 MiB",
        ));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

pub fn write_message<W: Write>(writer: &mut W, value: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ScriptedClient;
    use crate::tools::Session;
    use std::io::Cursor;

    #[test]
    fn initialize_and_a_confirm_gated_call_round_trip_offline() {
        let mut session = Session::new(ScriptedClient::default(), None).unwrap();
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}
        });
        let response = handle(&mut session, &init).unwrap();
        assert_eq!(response["result"]["serverInfo"]["name"], "fakescan");
        assert!(response["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("confirm=true"));
        assert!(session.client.calls.is_empty());

        let call = json!({
            "jsonrpc": "2.0",
            "id": "p",
            "method": "tools/call",
            "params": {"name": "send_faults", "arguments": {"fault": {"jam": true}}}
        });
        let response = handle(&mut session, &call).unwrap();
        assert_eq!(response["id"], "p");
        assert_eq!(response["result"]["isError"], false);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["sent"], false);
        assert_eq!(body["plan"]["body"]["jam"], true);
        assert!(session.client.calls.is_empty());

        let framed = {
            let mut buf = Vec::new();
            write_message(&mut buf, &init).unwrap();
            buf
        };
        let mut cursor = Cursor::new(framed);
        let body = read_message(&mut cursor).unwrap().unwrap();
        let again: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(again["method"], "initialize");
    }
}
