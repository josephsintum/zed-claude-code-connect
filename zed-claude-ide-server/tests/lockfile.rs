use serde_json::Value;
use std::path::Path;
use zed_claude_ide_server::lockfile::{LockDir, LockFile, RawLock};

#[test]
fn the_directory_is_the_override_then_the_claude_config_dir_then_home() {
    use std::ffi::OsString;
    let over = LockDir::resolve(Some(OsString::from("x/y")), Some(OsString::from("/c"))).unwrap();
    assert_eq!(over.path(), std::path::Path::new("x/y"));

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

#[test]
fn the_port_is_in_the_file_name() {
    let dir = LockDir::at("/tmp/ide");
    assert_eq!(
        dir.lock_path(54321),
        std::path::Path::new("/tmp/ide/54321.lock")
    );
}

#[test]
fn we_write_exactly_the_keys_the_cli_parses() {
    let lock = LockFile::for_this_process(Path::new("/Users/you/project"), "token-123");
    let v: Value = serde_json::to_value(&lock).unwrap();

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
        serde_json::json!(["/Users/you/project"])
    );
    assert_eq!(v["ideName"], "Zed");
    assert_eq!(v["transport"], "ws");
    assert_eq!(v["runningInWindows"], false);
    assert_eq!(v["authToken"], "token-123");
}

#[test]
fn a_lenient_read_tolerates_a_strangers_lock() {
    let raw: RawLock = serde_json::from_str(r#"{"ideName":"VS Code"}"#).unwrap();
    assert_eq!(raw.ide_name, "VS Code");
    assert_eq!(raw.pid, 0);
    assert!(raw.workspace_folders.is_empty());
    assert!(raw.auth_token.is_empty());
}

#[test]
fn writing_and_removing_a_lock() {
    let dir = tempfile::tempdir().unwrap();
    let lock_dir = LockDir::at(dir.path().join("ide"));
    let lock = LockFile::for_this_process(Path::new("/p"), "t");

    lock_dir.write(54321, &lock).unwrap();
    let path = lock_dir.lock_path(54321);
    assert!(path.exists());

    lock_dir.remove(54321).unwrap();
    assert!(!path.exists());

    lock_dir
        .remove(54321)
        .expect("removing a lock that is already gone is not an error");
}

#[test]
fn no_temp_file_is_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let lock_dir = LockDir::at(dir.path());
    lock_dir
        .write(54321, &LockFile::for_this_process(Path::new("/p"), "t"))
        .unwrap();

    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[cfg(unix)]
#[test]
fn the_lock_and_its_directory_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let ide = dir.path().join("ide");
    // Pre-create it world-readable: prepare() must tighten it, not just accept it.
    std::fs::create_dir_all(&ide).unwrap();
    std::fs::set_permissions(&ide, std::fs::Permissions::from_mode(0o755)).unwrap();

    let lock_dir = LockDir::at(&ide);
    lock_dir
        .write(54321, &LockFile::for_this_process(Path::new("/p"), "t"))
        .unwrap();

    let dir_mode = std::fs::metadata(&ide).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "the directory holds files containing tokens"
    );

    let file_mode = std::fs::metadata(lock_dir.lock_path(54321))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(file_mode, 0o600, "the lock file contains a bearer token");
}
