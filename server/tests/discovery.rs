//! Other editors write lock files into the same directory. VS Code's Claude
//! extension does, and will for the same project when both editors are open, so
//! discovery has to pick out our own live companion rather than the newest lock.

use std::path::Path;

use claude_code_connect::discovery::lock_for;
use claude_code_connect::lockfile::LockDir;
use serde_json::json;

fn write_lock(dir: &Path, port: u16, ide: &str, pid: u32, folder: &str) {
    std::fs::write(
        dir.join(format!("{port}.lock")),
        json!({
            "pid": pid,
            "workspaceFolders": [folder],
            "ideName": ide,
            "transport": "ws",
            "runningInWindows": false,
            "authToken": format!("token-for-{port}")
        })
        .to_string(),
    )
    .unwrap();
}

/// A pid that is certainly not running, for the stale-lock cases.
fn dead_pid() -> u32 {
    99_999
}

#[tokio::test]
async fn another_editors_lock_for_the_same_project_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let locks = LockDir::at(dir.path());
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();

    // VS Code's lock is written second, so it is the newest.
    write_lock(
        dir.path(),
        1111,
        "Zed",
        std::process::id(),
        project.to_str().unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_lock(
        dir.path(),
        2222,
        "Visual Studio Code",
        std::process::id(),
        project.to_str().unwrap(),
    );

    let found = lock_for(&locks, &project).unwrap();
    assert_eq!(found.port, 1111, "must pick ours, not the newest");
    assert_eq!(found.ide_name, "Zed");
}

#[tokio::test]
async fn a_lock_whose_process_has_died_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let locks = LockDir::at(dir.path());
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();

    write_lock(
        dir.path(),
        3333,
        "Zed",
        std::process::id(),
        project.to_str().unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_lock(
        dir.path(),
        4444,
        "Zed",
        dead_pid(),
        project.to_str().unwrap(),
    );

    let found = lock_for(&locks, &project).unwrap();
    assert_eq!(found.port, 3333, "a stale lock must not be dialled");
}

#[tokio::test]
async fn a_subdirectory_resolves_to_its_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let locks = LockDir::at(dir.path());
    let project = dir.path().join("proj");
    let nested = project.join("src").join("deep");
    std::fs::create_dir_all(&nested).unwrap();

    write_lock(
        dir.path(),
        5555,
        "Zed",
        std::process::id(),
        project.to_str().unwrap(),
    );

    assert_eq!(lock_for(&locks, &nested).unwrap().port, 5555);
}

#[tokio::test]
async fn a_sibling_project_is_not_a_match() {
    let dir = tempfile::tempdir().unwrap();
    let locks = LockDir::at(dir.path());
    let project = dir.path().join("proj");
    // A sibling whose path shares the prefix "proj" as a string but not as a path.
    let sibling = dir.path().join("proj-other");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();

    write_lock(
        dir.path(),
        6666,
        "Zed",
        std::process::id(),
        project.to_str().unwrap(),
    );

    assert!(
        lock_for(&locks, &sibling).is_err(),
        "prefix matching must respect path separators"
    );
}
