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
    /// How far a slow CLI may fall behind the editor before it starts skipping
    /// events. Only the newest selection matters, so skipping is harmless.
    pub event_capacity: usize,
}

/// Zed asks for code actions on every cursor move, so selection updates arrive as
/// fast as you can hold down shift-arrow.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(300);

/// Default for [`Config::event_capacity`].
pub const DEFAULT_EVENT_CAPACITY: usize = 100;

impl Config {
    pub fn new(worktree: impl Into<PathBuf>, lock_dir: LockDir) -> Self {
        Self {
            worktree: worktree.into(),
            lock_dir,
            port: None,
            debounce: DEFAULT_DEBOUNCE,
            event_capacity: DEFAULT_EVENT_CAPACITY,
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

    /// Mainly for tests: a small capacity makes a lagging subscriber reachable in a
    /// handful of publishes instead of relying on out-publishing the default.
    pub fn with_event_capacity(mut self, event_capacity: usize) -> Self {
        self.event_capacity = event_capacity;
        self
    }
}
