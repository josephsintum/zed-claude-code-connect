//! The editor side: Zed speaks LSP to this process over stdio.

mod documents;
mod framing;
mod protocol;
mod server;
mod watchdog;

pub use documents::DocumentStore;
pub use server::serve_lsp;
pub use watchdog::parent_exited;
