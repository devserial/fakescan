//! MCP server for a running fakescan. Optional: the scanner binary does not depend on it.
//!
//! The server speaks stdio JSON-RPC and calls the scanner's HTTP control API. It does not
//! start, stop, or embed a device.

mod error;
mod guard;
mod http;
mod rpc;
mod tools;

pub use http::{BaseUrl, HttpDevice};
pub use rpc::serve;
pub use tools::{capabilities, Session, INSTRUCTIONS};
