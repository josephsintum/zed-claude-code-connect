//! The companion as a value: start it, hold its handle, shut it down.
//!
//! The handle owns the lock file. Whatever happens to the handle happens to the
//! lock, which is what makes every exit route correct without each of them
//! remembering to clean up.

mod common;

use std::time::Duration;

use tokio::time::timeout;
use zed_claude_ide_server::companion::Companion;
use zed_claude_ide_server::config::Config;
use zed_claude_ide_server::lockfile::LockDir;

fn config(dir: &tempfile::TempDir) -> Config {
    Config::new(dir.path(), LockDir::at(dir.path()))
}

#[tokio::test]
async fn shutdown_removes_the_lock_file() {
    let dir = tempfile::tempdir().unwrap();
    let handle = Companion::start(config(&dir)).await.unwrap();
    let lock = handle.lock_path().to_path_buf();
    assert!(lock.exists());

    handle.shutdown().await;
    assert!(!lock.exists());
}

#[tokio::test]
async fn dropping_the_handle_removes_the_lock_file() {
    let dir = tempfile::tempdir().unwrap();
    let lock = {
        let handle = Companion::start(config(&dir)).await.unwrap();
        handle.lock_path().to_path_buf()
    };
    assert!(
        !lock.exists(),
        "an early return or a panic drops the handle; the lock must go with it"
    );
}

#[tokio::test]
async fn shutdown_closes_connected_clients() {
    let dir = tempfile::tempdir().unwrap();
    let handle = Companion::start(config(&dir)).await.unwrap();
    let mut cli = common::FakeCli::connect(handle.port(), handle.auth_token()).await;
    cli.handshake().await;

    handle.shutdown().await;

    assert!(
        cli.closed_within(Duration::from_secs(3)).await,
        "a connected CLI must see the socket close, not hang"
    );
}

#[tokio::test]
async fn a_new_connection_is_refused_after_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let handle = Companion::start(config(&dir)).await.unwrap();
    let port = handle.port();
    handle.shutdown().await;

    let attempt = timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await;
    assert!(
        matches!(attempt, Ok(Err(_))),
        "the listener must be gone after shutdown"
    );
}

#[tokio::test]
async fn stopped_resolves_once_the_companion_has_shut_down() {
    let dir = tempfile::tempdir().unwrap();
    let handle = Companion::start(config(&dir)).await.unwrap();
    let stopped = handle.stopped();
    let shutdown = handle.shutdown();
    let (_, ()) = tokio::join!(
        async { timeout(Duration::from_secs(3), stopped).await.unwrap() },
        shutdown
    );
}
