//! The CLI side: a loopback WebSocket speaking MCP JSON-RPC 2.0.

pub(crate) mod connection;
pub mod protocol;
pub(crate) mod server;
pub mod wire;
