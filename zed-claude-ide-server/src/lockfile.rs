//! The lock file: how the `claude` CLI finds a running companion.
//!
//! The CLI scans a directory for `<port>.lock` and takes the port from the file
//! *name*. A port field inside the file is ignored.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

// The directory lock files live in.
#[derive(Debug, Clone)]
pub struct LockDir {
    path: PathBuf,
}

impl LockDir {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Choose the directory from already-read environment values, so the library
    /// itself never touches the environment (tests inject a directory instead):
    ///
    /// 1. `ZED_CLAUDE_IDE_DIR` -- an explicit override.
    /// 2. `$CLAUDE_CONFIG_DIR/ide` -- the CLI scans this as well as the default
    ///    when the variable is set, and this project's tooling must agree with it.
    /// 3. `~/.claude/ide`.
    pub fn resolve(
        zed_override: Option<OsString>,
        claude_config_dir: Option<OsString>,
    ) -> io::Result<Self> {
        if let Some(dir) = zed_override.filter(|d| !d.is_empty()) {
            return Ok(Self::at(dir));
        }
        if let Some(dir) = claude_config_dir.filter(|d| !d.is_empty()) {
            return Ok(Self::at(PathBuf::from(dir).join("ide")));
        }
        let home = dirs::home_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?;
        Ok(Self::at(home.join(".claude").join("ide")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lock_path(&self, port: u16) -> PathBuf {
        self.path.join(format!("{port}.lock"))
    }
}
