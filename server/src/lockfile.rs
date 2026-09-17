//! The lock file: how the `claude` CLI finds a running companion.
//!
//! The CLI scans a directory for `<port>.lock`, takes the port from the file
//! *name*, and trusts the file when its `pid` is alive and the working directory
//! lies under one of its `workspaceFolders`. The body carries a bearer token, so
//! the file is `0600` inside a `0700` directory and written atomically.
//!
//! Two types on purpose. [`LockFile`] is what this companion writes: exactly the
//! six keys the CLI parses, pinned by a test, because the CLI is the consumer.
//! [`RawLock`] is what this project reads back: every field defaulted, because
//! other editors write into the same directory (VS Code's extension does) and a
//! stranger's lock must be skipped, not choked on. Strict out, lenient in.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Shown in the CLI's `/ide` picker, and how this project's own tooling tells its
/// lock files apart from those of any other editor open on the same project.
pub const IDE_NAME: &str = "Zed";

/// What this companion writes. The key set is a contract with the CLI.
#[derive(Debug, Serialize, Deserialize)]
pub struct LockFile {
    pub pid: u32,
    #[serde(rename = "workspaceFolders")]
    pub workspace_folders: Vec<String>,
    #[serde(rename = "ideName")]
    pub ide_name: String,
    pub transport: String,
    #[serde(rename = "runningInWindows")]
    pub running_in_windows: bool,
    #[serde(rename = "authToken")]
    pub auth_token: String,
}

impl LockFile {
    /// A lock for this process serving `worktree` with `auth_token`.
    pub fn for_this_process(worktree: &Path, auth_token: &str) -> Self {
        Self {
            pid: std::process::id(),
            workspace_folders: vec![worktree.to_string_lossy().into_owned()],
            ide_name: IDE_NAME.to_string(),
            transport: "ws".to_string(),
            running_in_windows: false,
            auth_token: auth_token.to_string(),
        }
    }
}

/// What this project reads from any lock in the directory, ours or not.
#[derive(Debug, Clone, Deserialize)]
pub struct RawLock {
    #[serde(rename = "workspaceFolders", default)]
    pub workspace_folders: Vec<String>,
    #[serde(rename = "authToken", default)]
    pub auth_token: String,
    #[serde(rename = "ideName", default)]
    pub ide_name: String,
    #[serde(default)]
    pub pid: u32,
}

/// The directory lock files live in.
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

    /// Create the directory if needed and make it owner-only. Tightens an
    /// existing directory too: it may predate this server, or another tool may
    /// have created it with the default umask.
    pub fn prepare(&self) -> io::Result<()> {
        fs::create_dir_all(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Write the lock for `port`, replacing any stale one, and hand back the guard
    /// that removes it on drop.
    ///
    /// Written to a temporary name then renamed, so a reader can never observe a
    /// half-written lock, and the rename replaces a leftover from a crashed
    /// process atomically.
    pub fn write(&self, port: u16, lock: &LockFile) -> io::Result<LockGuard> {
        self.prepare()?;
        let final_path = self.lock_path(port);
        let tmp_path = self.path.join(format!(".{port}.lock.tmp"));
        let _ = fs::remove_file(&tmp_path);

        // Compact, matching the byte shape the VS Code extension writes.
        let json = serde_json::to_vec(lock).map_err(io::Error::other)?;
        {
            use std::io::Write;
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut f = options.open(&tmp_path)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        fs::rename(&tmp_path, &final_path)?;
        Ok(LockGuard { path: final_path })
    }

    /// Remove the lock for `port` if it exists.
    pub fn remove(&self, port: u16) -> io::Result<()> {
        remove_ignoring_missing(&self.lock_path(port))
    }
}

impl LockDir {
    /// Remove every lock in the directory that carries `ide_name` and names a
    /// process that no longer exists. Returns the ports removed.
    ///
    /// A companion can be killed before its own cleanup runs -- Zed sends `exit`
    /// and kills in the same instant -- and the CLI lists every lock matching the
    /// working directory in its /ide picker, dead or not. The next companion to
    /// start tidies up after the last. Only our own name: a stranger's lock is
    /// not ours to judge. Only dead pids: a live one belongs to another window.
    pub fn sweep_dead(&self, ide_name: &str) -> Vec<u16> {
        let Ok(entries) = fs::read_dir(&self.path) else {
            return Vec::new();
        };
        let mut swept = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lock") {
                continue;
            }
            let Some(port) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u16>().ok())
            else {
                continue;
            };
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(raw) = serde_json::from_str::<RawLock>(&text) else {
                continue;
            };
            if raw.ide_name == ide_name
                && !pid_is_alive(raw.pid)
                && remove_ignoring_missing(&path).is_ok()
            {
                tracing::info!(
                    "removed stale lock {} left by dead pid {}",
                    path.display(),
                    raw.pid
                );
                swept.push(port);
            }
        }
        swept
    }
}

/// Same liveness test the CLI applies before trusting a lock: signal 0 probes
/// for the process without touching it.
pub fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Owns a written lock file; dropping it removes the file.
///
/// Every exit route the owner can take -- normal return, `?`, panic unwind, a
/// test dropping its harness -- goes through `Drop`, which is the point: lock
/// removal used to live in three places and none of them was the watchdog.
#[derive(Debug)]
#[must_use = "dropping the guard removes the lock file immediately"]
pub struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Err(e) = remove_ignoring_missing(&self.path) {
            tracing::warn!("could not remove {}: {}", self.path.display(), e);
        }
    }
}

/// A lock that is already gone is not an error: a concurrent cleanup, or the CLI
/// unlinking a lock it judged stale, both leave nothing to do.
fn remove_ignoring_missing(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
