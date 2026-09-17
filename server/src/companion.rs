//! The companion as one value: start it, hold the handle, shut it down.
//!
//! `Companion::start` binds the socket, writes the lock file, and spawns the
//! tracker and the accept loop under one cancellation token. The [`Handle`] owns
//! the lock-file guard, so every way the handle can go away -- an explicit
//! `shutdown`, a `?` in the caller, a panic unwinding, a test finishing -- removes
//! the lock. Lock removal used to live in three places and none of them was the
//! watchdog's exit; now it lives in `Drop`.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use std::io::{BufRead, Write};
use tokio::task::JoinSet;
use tokio_util::sync::{CancellationToken, DropGuard, WaitForCancellationFutureOwned};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::Config;
use crate::lockfile::{LockFile, LockGuard, IDE_NAME};
use crate::lsp;
use crate::mcp;
use crate::selection::{EventBus, SelectionTracker};

/// How long `shutdown` waits for the accept loop and tracker to wind down.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

pub struct Companion;

impl Companion {
    /// Bind, write the lock, and start serving CLI connections. The LSP side is
    /// not started here: call [`Handle::run_lsp`] with the streams Zed speaks on.
    pub async fn start(config: Config) -> Result<Handle> {
        // Tidy up after any companion that was killed before it could.
        let swept = config.lock_dir.sweep_dead(IDE_NAME);
        if !swept.is_empty() {
            info!(
                "swept {} stale lock file(s): ports {:?}",
                swept.len(),
                swept
            );
        }

        let (listener, port) = mcp::server::bind(config.port).await?;
        let auth_token = Uuid::new_v4().to_string();
        let lock = config.lock_dir.write(
            port,
            &LockFile::for_this_process(&config.worktree, &auth_token),
        )?;
        info!(
            "listening on 127.0.0.1:{} for {}; lock {}",
            port,
            config.worktree.display(),
            lock.path().display()
        );

        let bus = EventBus::new(config.event_capacity);
        let (tracker, tracker_task) = SelectionTracker::new(bus.clone(), config.debounce);
        let cancel = CancellationToken::new();
        let mut tasks = JoinSet::new();

        // The tracker task otherwise lives as long as any tracker clone, and the
        // handle keeps one; the token is what ends it.
        let tracker_cancel = cancel.clone();
        tasks.spawn(async move {
            tokio::select! {
                _ = tracker_task => {}
                _ = tracker_cancel.cancelled() => {}
            }
        });
        tasks.spawn(mcp::server::serve(
            listener,
            auth_token.clone(),
            config.worktree.clone(),
            bus.clone(),
            cancel.clone(),
        ));

        Ok(Handle {
            port,
            auth_token,
            lock,
            _cancel_on_drop: cancel.clone().drop_guard(),
            cancel,
            tasks,
            bus,
            tracker,
        })
    }
}

/// A running companion. Dropping it stops everything and removes the lock.
pub struct Handle {
    port: u16,
    auth_token: String,
    lock: LockGuard,
    cancel: CancellationToken,
    /// Cancels the token when the handle is dropped without `shutdown`.
    _cancel_on_drop: DropGuard,
    tasks: JoinSet<()>,
    bus: EventBus,
    tracker: SelectionTracker,
}

impl Handle {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    pub fn lock_path(&self) -> &Path {
        self.lock.path()
    }

    /// What every connected CLI is told; tests publish into it directly.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// What the editor side feeds; a fake Zed drives it.
    pub fn tracker(&self) -> &SelectionTracker {
        &self.tracker
    }

    /// Serve LSP on these streams until the input ends, then begin shutting down:
    /// Zed closing our stdin is the normal way a session ends.
    ///
    /// The loop is blocking and gets an OS thread of its own, so a cursor move is
    /// answered by the thread that read it rather than handed to the runtime and
    /// back. `tokio::io::stdin()` does exactly that handoff -- it reads on the
    /// blocking pool -- and it cost 12.1us per round trip, with one further
    /// scheduler hop costing 6.4us more. The returned future resolves when the
    /// thread does.
    pub fn run_lsp<R, W>(&self, input: R, output: W) -> impl std::future::Future<Output = ()>
    where
        R: BufRead + Send + 'static,
        W: Write + Send + 'static,
    {
        let tracker = self.tracker.clone();
        let cancel = self.cancel.clone();
        let (done, finished) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("lsp".to_string())
            .spawn(move || {
                lsp::serve_lsp(input, output, tracker);
                // The receiver is gone if the companion stopped for another
                // reason first; there is then nobody left to tell.
                let _ = done.send(());
            })
            .expect("spawn the LSP thread");
        async move {
            let _ = finished.await;
            cancel.cancel();
        }
    }

    /// Resolves once the companion is stopping, for whatever reason: the accept
    /// loop giving up, the LSP input ending, or `shutdown` being called.
    pub fn stopped(&self) -> WaitForCancellationFutureOwned {
        self.cancel.clone().cancelled_owned()
    }

    /// Remove the lock file, stop accepting, and close connected clients.
    ///
    /// The lock goes first. It is the one thing another process reads, and when
    /// Zed is the reason we are stopping it kills us moments after sending
    /// `exit`; everything after this line is best effort against that clock.
    pub async fn shutdown(self) {
        let Handle {
            lock,
            cancel,
            _cancel_on_drop,
            mut tasks,
            ..
        } = self;
        // `_cancel_on_drop` is named rather than swallowed by `..`, because `..`
        // drops it at this statement -- cancelling before either line below runs.
        // Bound here, its Drop waits until the end of the function, so these two
        // lines are the only things that unlink or cancel, in that order.
        drop(lock);
        cancel.cancel();
        let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            warn!("companion tasks did not stop within {:?}", SHUTDOWN_GRACE);
        }
    }
}
