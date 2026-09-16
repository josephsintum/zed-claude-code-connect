//! Companion server for the Zed <-> Claude Code IDE bridge.
//!
//! Zed speaks LSP to this process over stdio (`lsp`); the `claude` CLI speaks MCP
//! to it over a loopback WebSocket (`mcp`); the CLI finds it through a lock file
//! (`lockfile`, `discovery`). `selection` is the one fact both sides care about,
//! and `companion` is the whole thing as a value you can start and stop.
//!
//! Exposed as a library so the integration tests can boot the companion
//! in-process, play Zed on one side and the CLI on the other, and assert what crosses.

pub mod lockfile;
