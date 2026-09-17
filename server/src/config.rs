//! Everything the companion needs to know at startup, decided once by `main`.
//!
//! The library reads no environment variables and never guesses a working
//! directory: whoever starts a companion says which worktree it serves and where
//! its lock file goes. Tests hand in a temporary directory and stay isolated from
//! each other and from any real `~/.claude/ide`.

use std::path::PathBuf;
use std::time::Duration;

use crate::lockfile::LockDir;

#[derive(Debug, Clone)]
pub struct Config {
    /// The project this companion serves; goes into the lock file's
    /// `workspaceFolders` and is what `getWorkspaceFolders` reports.
    pub worktree: PathBuf,
    /// Where the lock file is written.
    pub lock_dir: LockDir,
    /// Bind this port rather than a random one from the protocol's range.
    pub port: Option<u16>,
    /// How long a selection must hold still before it is sent. Matches the VS
    /// Code extension; tests shorten it.
    pub debounce: Duration,
}

/// Zed asks for code actions on every cursor move, so selection updates arrive as
/// fast as you can hold down shift-arrow.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(300);

impl Config {
    pub fn new(worktree: impl Into<PathBuf>, lock_dir: LockDir) -> Self {
        Self {
            worktree: worktree.into(),
            lock_dir,
            port: None,
            debounce: DEFAULT_DEBOUNCE,
        }
    }

    pub fn with_debounce(mut self, debounce: Duration) -> Self {
        self.debounce = debounce;
        self
    }

    pub fn with_port(mut self, port: Option<u16>) -> Self {
        self.port = port;
        self
    }
}
