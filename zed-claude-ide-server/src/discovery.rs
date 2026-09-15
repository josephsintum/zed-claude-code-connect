//! Finding a running companion the same way the `claude` CLI does.
//!
//! Shared by the `at-mention` subcommand and the `watch` example so both agree
//! with the CLI on what counts as the companion for a given directory.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::lockfile::{pid_is_alive, LockDir, RawLock, IDE_NAME};

#[derive(Debug, Clone)]
pub struct IdeLock {
    /// Taken from the file name, never from the JSON -- same as the CLI.
    pub port: u16,
    pub auth_token: String,
    pub workspace_folders: Vec<String>,
    pub ide_name: String,
    pub pid: u32,
}

/// Every lock file currently present in `dir`, newest first.
pub fn all_locks(dir: &LockDir) -> Result<Vec<IdeLock>> {
    let mut entries: Vec<(std::time::SystemTime, IdeLock)> = Vec::new();

    let Ok(read_dir) = std::fs::read_dir(dir.path()) else {
        return Ok(Vec::new());
    };

    for entry in read_dir.flatten() {
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
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(raw) = serde_json::from_str::<RawLock>(&text) else {
            continue;
        };
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        entries.push((
            mtime,
            IdeLock {
                port,
                auth_token: raw.auth_token,
                workspace_folders: raw.workspace_folders,
                ide_name: raw.ide_name,
                pid: raw.pid,
            },
        ));
    }

    entries.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    Ok(entries.into_iter().map(|(_, l)| l).collect())
}

/// Our companion covering `path`, using the CLI's containment rule: the directory
/// is the workspace folder itself, or lies beneath it.
///
/// Other editors write lock files into the same directory -- VS Code's extension
/// does, and will for the same project if both are open -- so a lock only counts
/// as ours if it carries our ide name. Dead ones are skipped rather than dialled.
pub fn lock_for(dir: &LockDir, path: &Path) -> Result<IdeLock> {
    let target: PathBuf = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    for lock in all_locks(dir)? {
        if lock.ide_name != IDE_NAME || !pid_is_alive(lock.pid) {
            continue;
        }
        for folder in &lock.workspace_folders {
            let folder_path = Path::new(folder);
            let folder_canon = folder_path
                .canonicalize()
                .unwrap_or_else(|_| folder_path.to_path_buf());
            if target == folder_canon || target.starts_with(&folder_canon) {
                return Ok(lock);
            }
        }
    }

    Err(anyhow!(
        "no running companion covers {} -- is the project open in Zed?",
        target.display()
    ))
}
