//! Shared test rig: a companion booted in-process with a fake Zed on one side
//! and a fake `claude` CLI on the other.

#![allow(dead_code)]

pub mod fake_cli;
pub mod fake_zed;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use claude_code_connect::companion::{Companion, Handle};
use claude_code_connect::config::Config;
use claude_code_connect::lockfile::LockDir;
use claude_code_connect::selection::EventBus;

pub use fake_cli::FakeCli;
pub use fake_zed::FakeZed;

/// One throwaway lock directory for the whole test binary, handed to each server
/// through its config. Ports differ, so lock names never collide.
pub fn ide_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

pub struct Rig {
    /// The running companion. Dropping the rig stops it and removes its lock.
    pub handle: Handle,
    /// A fresh worktree directory for this test; files for the fake editor go here.
    pub worktree: PathBuf,
    pub port: u16,
    pub token: String,
    pub bus: EventBus,
    pub zed: FakeZed,
    /// What the server advertised in its `initialize` response.
    pub capabilities: Value,
    /// Connected and past the handshake before any selection is made, so every
    /// `selection_changed` the test causes is observed live rather than replayed.
    pub cli: FakeCli,
    _worktree_dir: tempfile::TempDir,
}

/// Boot the companion the way `serve` wires it: one broadcast channel from the
/// LSP side to the socket side, a fake Zed attached over an in-memory pipe and
/// initialized, a fake CLI connected and through its handshake.
pub async fn boot() -> Rig {
    let worktree_dir = tempfile::tempdir().unwrap();
    let worktree = worktree_dir.path().canonicalize().unwrap();

    // Short debounce: these tests wait for real time to pass.
    let config = Config::new(worktree.clone(), LockDir::at(ide_dir()))
        .with_debounce(Duration::from_millis(50));
    let handle = Companion::start(config).await.expect("companion starts");
    let port = handle.port();
    let token = handle.auth_token().to_string();
    let bus = handle.bus().clone();

    let mut zed = FakeZed::start(handle.tracker().clone());
    let capabilities = zed.initialize(&worktree).await;

    let mut cli = FakeCli::connect(port, &token).await;
    cli.handshake().await;

    Rig {
        handle,
        worktree,
        port,
        token,
        bus,
        zed,
        capabilities,
        cli,
        _worktree_dir: worktree_dir,
    }
}

impl Rig {
    /// Write `text` to `name` inside the worktree and return its URI.
    pub fn file(&self, name: &str, text: &str) -> lsp_types::Url {
        let path = self.worktree.join(name);
        std::fs::write(&path, text).unwrap();
        lsp_types::Url::from_file_path(path).unwrap()
    }
}

pub fn range(sl: u32, sc: u32, el: u32, ec: u32) -> lsp_types::Range {
    use lsp_types::{Position, Range};
    Range {
        start: Position {
            line: sl,
            character: sc,
        },
        end: Position {
            line: el,
            character: ec,
        },
    }
}
