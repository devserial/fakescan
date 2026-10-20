//! `fakescan-mcp` — stdio MCP server. Attach to a scanner that is already listening.

use std::io::{BufReader, Write};
use std::process::ExitCode;
use std::time::Duration;

use fakescan_mcp::{serve, BaseUrl, HttpDevice, Session};

fn main() -> ExitCode {
    let config = match Config::from_env_args() {
        Ok(Some(config)) => config,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("fakescan-mcp: {message}");
            eprintln!("Try fakescan-mcp --help");
            return ExitCode::from(2);
        }
    };
    let client = HttpDevice::new(config.base, config.timeout);
    let mut session = match Session::new(client, config.device) {
        Ok(session) => session,
        Err(message) => {
            eprintln!("fakescan-mcp: {message}");
            return ExitCode::from(2);
        }
    };
    let stdin = BufReader::new(std::io::stdin());
    let stdout = std::io::stdout();
    if let Err(error) = serve(&mut session, stdin, stdout) {
        eprintln!("fakescan-mcp: {error}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

struct Config {
    base: BaseUrl,
    device: Option<String>,
    timeout: Duration,
}

impl Config {
    /// `Ok(None)` means help was printed.
    fn from_env_args() -> Result<Option<Self>, String> {
        let mut args = std::env::args().skip(1);
        let mut base = std::env::var("FAKESCAN_BASE_URL").ok();
        let mut device = std::env::var("FAKESCAN_DEVICE")
            .ok()
            .filter(|s| !s.is_empty());
        let mut timeout_ms = 5_000u64;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => {
                    print_help();
                    return Ok(None);
                }
                "--base-url" => {
                    base = Some(args.next().ok_or("--base-url needs a value")?);
                }
                "--device" => {
                    device = Some(args.next().ok_or("--device needs a value")?);
                }
                "--timeout-ms" => {
                    let raw = args.next().ok_or("--timeout-ms needs a value")?;
                    timeout_ms = raw
                        .parse()
                        .map_err(|_| format!("--timeout-ms {raw:?} is not an integer"))?;
                    if timeout_ms == 0 || timeout_ms > 120_000 {
                        return Err("--timeout-ms must be from 1 to 120000".into());
                    }
                }
                other if other.starts_with("--base-url=") => {
                    base = Some(other.trim_start_matches("--base-url=").to_string());
                }
                other if other.starts_with("--device=") => {
                    device = Some(other.trim_start_matches("--device=").to_string());
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        let base = BaseUrl::parse(base.as_deref().unwrap_or("http://127.0.0.1:3920"))?;
        Ok(Some(Self {
            base,
            device,
            timeout: Duration::from_millis(timeout_ms),
        }))
    }
}

fn print_help() {
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "\
fakescan-mcp — MCP server for a running fakescan (stdio)

Usage: fakescan-mcp [--base-url URL] [--device ID] [--timeout-ms N]

  --base-url     Control API origin. Default http://127.0.0.1:3920
                 or FAKESCAN_BASE_URL. http only, no path, no userinfo.
  --device       Optional device id (FAKESCAN_DEVICE). Calls go to /d/{{id}}/...
  --timeout-ms   Per-request timeout, 1 to 120000. Default 5000.

The server does not start or stop fakescan. Writes stay in the working
directory, the temp directory, /workspace, or FAKESCAN_WRITE_DIRS.
"
    );
}
