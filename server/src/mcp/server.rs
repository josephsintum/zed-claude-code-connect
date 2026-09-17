//! The MCP server's socket: binding the loopback port the CLI will dial, and
//! accepting connections until told to stop.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::connection::handle_connection;
use crate::selection::EventBus;

// The VS Code extension draws a random port from this range and probes it for
// availability; the CLI reads the port from the lock file *name*, so any free port
// works. Random beats a sequential scan here: several worktrees start servers at
// once and a shared low base collides constantly.
const DEFAULT_PORT_START: u16 = 10_000;
const DEFAULT_PORT_END: u16 = 65_535;
const PORT_ATTEMPTS: usize = 100;

/// Try to bind to a port in the given range, returning the listener and the actual port
async fn find_available_port(
    preferred_port: Option<u16>,
    port_start: u16,
    port_end: u16,
) -> Result<(TcpListener, u16)> {
    // If a specific port is requested, try it first
    if let Some(port) = preferred_port {
        let addr = format!("127.0.0.1:{}", port);
        if let Ok(listener) = TcpListener::bind(&addr).await {
            info!("Bound to requested port {}", port);
            return Ok((listener, port));
        }
        warn!(
            "Requested port {} is unavailable, trying dynamic allocation",
            port
        );
    }

    // Random draws from the range, then let the OS choose as a last resort.
    use rand::Rng;
    for _ in 0..PORT_ATTEMPTS {
        let port = rand::thread_rng().gen_range(port_start..=port_end);
        let addr = format!("127.0.0.1:{}", port);
        if let Ok(listener) = TcpListener::bind(&addr).await {
            info!("Found available port: {}", port);
            return Ok((listener, port));
        }
    }

    if let Ok(listener) = TcpListener::bind("127.0.0.1:0").await {
        let port = listener.local_addr()?.port();
        warn!("Falling back to OS-assigned port {}", port);
        return Ok((listener, port));
    }

    Err(anyhow!(
        "No available ports in range {}-{}",
        port_start,
        port_end
    ))
}

/// Bind the loopback listener the CLI will dial.
pub(crate) async fn bind(preferred_port: Option<u16>) -> Result<(TcpListener, u16)> {
    find_available_port(preferred_port, DEFAULT_PORT_START, DEFAULT_PORT_END).await
}

/// Accept CLI connections until `cancel` fires or the listener becomes unusable.
///
/// Each connection runs under a child token, so cancelling the parent closes
/// every client with a 1001 (going away) frame rather than a dropped socket.
/// Giving up on the listener cancels the token too: a companion that cannot
/// accept is not serving anything, and the caller's lock file should say so.
pub(crate) async fn serve(
    listener: TcpListener,
    auth_token: String,
    worktree: PathBuf,
    bus: EventBus,
    cancel: CancellationToken,
) {
    // `accept` failing is not the end of the listener. ECONNABORTED means one
    // client went away mid-handshake; EMFILE means the process is out of
    // descriptors right now. Treating either as end-of-loop took the whole
    // companion down, lock file and all, on the first one.
    let mut consecutive_failures: u32 = 0;
    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer_addr) = match accepted {
            Ok(accepted) => {
                consecutive_failures = 0;
                accepted
            }
            Err(e) => match accept_error_policy(&e) {
                AcceptErrorPolicy::Retry => {
                    debug!("accept: transient error, retrying: {}", e);
                    continue;
                }
                AcceptErrorPolicy::Backoff(delay) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_ACCEPT_FAILURES {
                        error!(
                            "accept failed {} times in a row, giving up; last error: {}",
                            consecutive_failures, e
                        );
                        cancel.cancel();
                        break;
                    }
                    warn!(
                        "accept failed ({}), backing off {:?} [{}/{}]",
                        e, delay, consecutive_failures, MAX_CONSECUTIVE_ACCEPT_FAILURES
                    );
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(delay) => continue,
                    }
                }
                AcceptErrorPolicy::Fatal => {
                    error!("listener is unusable: {}", e);
                    cancel.cancel();
                    break;
                }
            },
        };
        debug!("New connection from {}", peer_addr);
        tokio::spawn(handle_connection(
            stream,
            peer_addr,
            auth_token.clone(),
            bus.clone(),
            worktree.clone(),
            cancel.child_token(),
        ));
    }
    debug!("accept loop ended");
}

/// How the accept loop should react to an `accept()` error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptErrorPolicy {
    /// One connection failed on its own; the listener is fine. Try again at once.
    Retry,
    /// A resource is exhausted or the cause is unknown. Wait, then try again,
    /// counting towards `MAX_CONSECUTIVE_ACCEPT_FAILURES`.
    Backoff(std::time::Duration),
    /// The listener itself is gone. Nothing more will ever be accepted.
    Fatal,
}

/// Give up after this many consecutive `Backoff` failures. Plain "log and
/// continue" would spin at full CPU on a persistent EMFILE, which is worse than
/// exiting: with the loop gone the caller removes the lock file and the CLI
/// stops trying to dial a port nobody answers.
pub const MAX_CONSECUTIVE_ACCEPT_FAILURES: u32 = 20;

const RESOURCE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);
const UNKNOWN_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

pub fn accept_error_policy(e: &std::io::Error) -> AcceptErrorPolicy {
    use std::io::ErrorKind;

    match e.kind() {
        // The peer reset before we picked the connection up; nothing wrong here.
        ErrorKind::ConnectionAborted
        | ErrorKind::ConnectionReset
        | ErrorKind::Interrupted
        | ErrorKind::WouldBlock => return AcceptErrorPolicy::Retry,
        // Not a valid listener any more.
        ErrorKind::InvalidInput | ErrorKind::NotConnected => return AcceptErrorPolicy::Fatal,
        _ => {}
    }

    match e.raw_os_error() {
        Some(code)
            if code == libc::EMFILE
                || code == libc::ENFILE
                || code == libc::ENOBUFS
                || code == libc::ENOMEM =>
        {
            AcceptErrorPolicy::Backoff(RESOURCE_BACKOFF)
        }
        Some(code) if code == libc::EBADF || code == libc::EINVAL || code == libc::ENOTSOCK => {
            AcceptErrorPolicy::Fatal
        }
        _ => AcceptErrorPolicy::Backoff(UNKNOWN_BACKOFF),
    }
}

#[cfg(test)]
mod accept_policy_tests {
    use super::*;
    use std::io::Error;

    fn os(code: i32) -> Error {
        Error::from_raw_os_error(code)
    }

    #[test]
    fn a_client_that_went_away_mid_handshake_is_retried_at_once() {
        assert_eq!(
            accept_error_policy(&os(libc::ECONNABORTED)),
            AcceptErrorPolicy::Retry
        );
        assert_eq!(
            accept_error_policy(&os(libc::EINTR)),
            AcceptErrorPolicy::Retry
        );
    }

    #[test]
    fn resource_exhaustion_backs_off_instead_of_spinning() {
        for code in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert!(
                matches!(
                    accept_error_policy(&os(code)),
                    AcceptErrorPolicy::Backoff(d) if d >= RESOURCE_BACKOFF
                ),
                "errno {code} must back off"
            );
        }
    }

    #[test]
    fn a_dead_listener_is_fatal() {
        assert_eq!(
            accept_error_policy(&os(libc::EBADF)),
            AcceptErrorPolicy::Fatal
        );
        assert_eq!(
            accept_error_policy(&os(libc::EINVAL)),
            AcceptErrorPolicy::Fatal
        );
    }

    #[test]
    fn an_unknown_error_backs_off_and_counts_towards_the_cap() {
        let e = Error::other("something new");
        assert!(matches!(
            accept_error_policy(&e),
            AcceptErrorPolicy::Backoff(_)
        ));
    }
}
