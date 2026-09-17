//! The lock file is a wire contract: the CLI parses these exact keys, takes the
//! port from the *filename*, and checks that `pid` names a live process.
//!
//! Each test gets its own directory through the config, so none of them touches
//! the environment or a real `~/.claude/ide`.

use serde_json::Value;

use claude_code_connect::companion::{Companion, Handle};
use claude_code_connect::config::Config;
use claude_code_connect::lockfile::{LockDir, LockFile, RawLock};

/// Boot a companion whose lock directory is `dir`; returns the lock path, the
/// port, and the handle that must stay alive for the lock to exist.
async fn boot(dir: &std::path::Path) -> (std::path::PathBuf, u16, Handle) {
    let handle = Companion::start(Config::new(dir, LockDir::at(dir)))
        .await
        .unwrap();
    (handle.lock_path().to_path_buf(), handle.port(), handle)
}

#[tokio::test]
async fn lock_file_has_exactly_the_keys_the_cli_parses() {
    let dir = tempfile::tempdir().unwrap();
    let (lock, _port, _handle) = boot(dir.path()).await;
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();

    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "authToken",
            "ideName",
            "pid",
            "runningInWindows",
            "transport",
            "workspaceFolders"
        ],
        "an extra or missing key changes a contract the CLI parses"
    );

    assert!(v["pid"].is_u64());
    assert_eq!(
        v["workspaceFolders"],
        serde_json::json!([dir.path().to_str().unwrap()])
    );
    assert_eq!(
        v["ideName"], "Zed",
        "this string is what the CLI's /ide picker shows"
    );
    assert_eq!(v["transport"], "ws");
    assert_eq!(v["runningInWindows"], false);
    // A v4 UUID, as the VS Code extension mints.
    assert_eq!(v["authToken"].as_str().unwrap().len(), 36);
}

#[tokio::test]
async fn lock_file_names_a_live_process() {
    let dir = tempfile::tempdir().unwrap();
    let (lock, _port, _handle) = boot(dir.path()).await;
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
    // The CLI unlinks any lock whose pid is dead, so this must be our own.
    assert_eq!(v["pid"].as_u64().unwrap() as u32, std::process::id());
}

#[cfg(unix)]
#[tokio::test]
async fn lock_file_and_directory_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let ide = dir.path().join("ide");
    let (lock, _port, _handle) = boot(&ide).await;

    let file_mode = std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode, 0o600, "the lock file carries a bearer token");

    let dir_mode = std::fs::metadata(&ide).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700);
}

#[cfg(unix)]
#[tokio::test]
async fn a_preexisting_world_readable_directory_gets_tightened() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let ide = dir.path().join("ide");
    std::fs::create_dir_all(&ide).unwrap();
    std::fs::set_permissions(&ide, std::fs::Permissions::from_mode(0o755)).unwrap();

    let _handle = boot(&ide).await;

    let mode = std::fs::metadata(&ide).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o700,
        "a directory left world-readable must be tightened"
    );
}

#[tokio::test]
async fn bound_port_is_in_the_range_the_protocol_uses() {
    let dir = tempfile::tempdir().unwrap();
    let (_lock, port, _handle) = boot(dir.path()).await;
    assert!(
        (10_000..=65_535).contains(&port),
        "port {port} outside the 10000-65535 range the protocol uses"
    );
}

// ---- the lock module on its own ----

fn sample_lock(dir: &std::path::Path) -> LockFile {
    LockFile::for_this_process(dir, "token")
}

#[test]
fn dropping_the_guard_removes_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let locks = LockDir::at(dir.path());
    let guard = locks.write(12345, &sample_lock(dir.path())).unwrap();
    assert!(guard.path().exists());
    drop(guard);
    assert!(
        !locks.lock_path(12345).exists(),
        "the guard is the one owner of the lock; nothing else should have to remember"
    );
}

#[test]
fn the_guard_tolerates_a_file_that_is_already_gone() {
    // The CLI unlinks locks it judges stale, and a concurrent cleanup may win the
    // race; neither is an error worth surfacing.
    let dir = tempfile::tempdir().unwrap();
    let locks = LockDir::at(dir.path());
    let guard = locks.write(12345, &sample_lock(dir.path())).unwrap();
    std::fs::remove_file(guard.path()).unwrap();
    drop(guard); // must not panic
    assert!(locks.remove(12345).is_ok());
}

#[test]
fn writing_leaves_no_temporary_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let locks = LockDir::at(dir.path());
    let _guard = locks.write(2222, &sample_lock(dir.path())).unwrap();
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["2222.lock"],
        "write-then-rename must clean up: {names:?}"
    );
}

#[test]
fn writing_replaces_a_stale_lock_for_the_same_port() {
    let dir = tempfile::tempdir().unwrap();
    let locks = LockDir::at(dir.path());
    std::fs::write(
        locks.lock_path(3333),
        "{\"pid\": 1, \"authToken\": \"old\"}",
    )
    .unwrap();
    let _guard = locks.write(3333, &sample_lock(dir.path())).unwrap();
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(locks.lock_path(3333)).unwrap()).unwrap();
    assert_eq!(v["authToken"], "token");
}

#[test]
fn a_lenient_read_tolerates_a_strangers_lock() {
    // Another editor's lock may carry a subset of these keys, or extra ones.
    let raw: RawLock = serde_json::from_str("{\"pid\": 7, \"somethingElse\": true}").unwrap();
    assert_eq!(raw.pid, 7);
    assert!(raw.workspace_folders.is_empty());
    assert_eq!(raw.ide_name, "");
}

#[test]
fn the_directory_is_the_override_then_the_claude_config_dir_then_home() {
    use std::ffi::OsString;
    let over = LockDir::resolve(Some(OsString::from("/x/y")), Some(OsString::from("/c"))).unwrap();
    assert_eq!(over.path(), std::path::Path::new("/x/y"));

    let cfg = LockDir::resolve(None, Some(OsString::from("/c"))).unwrap();
    assert_eq!(cfg.path(), std::path::Path::new("/c/ide"));

    let empty_is_unset =
        LockDir::resolve(Some(OsString::new()), Some(OsString::from("/c"))).unwrap();
    assert_eq!(empty_is_unset.path(), std::path::Path::new("/c/ide"));

    let home = LockDir::resolve(None, None).unwrap();
    assert!(
        home.path().ends_with(".claude/ide"),
        "{}",
        home.path().display()
    );
}

/// A companion killed too fast to clean up -- Zed sends `exit` and kills within
/// the same instant -- leaves a lock the CLI lists in its /ide picker as a dead
/// "Zed" entry. The next companion to start sweeps those, and only those: a
/// stranger's lock is not ours to judge, and a live one of ours belongs to
/// another window.
#[tokio::test]
async fn starting_sweeps_our_dead_locks_and_leaves_everyone_elses() {
    let dir = tempfile::tempdir().unwrap();
    let locks = LockDir::at(dir.path());
    let write = |port: u16, ide: &str, pid: u32| {
        std::fs::write(
            locks.lock_path(port),
            serde_json::json!({
                "pid": pid, "workspaceFolders": ["/x"], "ideName": ide,
                "transport": "ws", "runningInWindows": false, "authToken": "t"
            })
            .to_string(),
        )
        .unwrap();
    };
    let dead = 99_999;
    write(1001, "Zed", dead);
    write(1002, "Zed", std::process::id());
    write(1003, "Visual Studio Code", dead);

    let _handle = Companion::start(Config::new(dir.path(), locks.clone()))
        .await
        .unwrap();

    assert!(!locks.lock_path(1001).exists(), "our dead lock is swept");
    assert!(
        locks.lock_path(1002).exists(),
        "our live lock is another window's"
    );
    assert!(
        locks.lock_path(1003).exists(),
        "a stranger's lock is left alone"
    );
}
