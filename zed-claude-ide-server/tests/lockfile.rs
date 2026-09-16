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
