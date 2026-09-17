//! Noticing that Zed has gone away without closing our stdin.

use std::time::Duration;

use tracing::warn;

/// Resolves when this process has been reparented, which on Unix means the
/// parent -- Zed -- has died. Polls every `interval`.
///
/// Zed normally ends a session by closing our stdin, and that path needs no
/// watchdog. This covers a crash, or the disconnect sometimes seen after a Mac
/// sleeps and wakes, where the pipe stays open with nobody on the other end.
/// Returning, rather than exiting the process here, lets the caller run the same
/// shutdown path as every other reason to stop.
pub async fn parent_exited(interval: Duration) {
    #[cfg(unix)]
    {
        use std::os::unix::process::parent_id;
        let initial = parent_id();
        loop {
            tokio::time::sleep(interval).await;
            let current = parent_id();
            // Reparenting is the whole signal: an orphan is handed to init (pid 1)
            // or, on macOS, launchd. Testing for pid 1 separately would be either
            // redundant -- it already differs from any ordinary parent -- or wrong,
            // firing on the first tick for a companion whose parent was init all
            // along, which is how it reads under a container entrypoint.
            if current != initial {
                warn!(
                    "parent process changed from {} to {}; Zed has gone away",
                    initial, current
                );
                return;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = interval;
        std::future::pending::<()>().await
    }
}
