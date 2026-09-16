use zed_claude_ide_server::lockfile::LockDir;

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
