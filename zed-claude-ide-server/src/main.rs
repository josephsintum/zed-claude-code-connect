use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

use zed_claude_ide_server::companion::Companion;
use zed_claude_ide_server::config::Config;
use zed_claude_ide_server::lockfile::LockDir;
use zed_claude_ide_server::{at_mention, discovery, lsp};

#[derive(Parser)]
#[command(
    name = "zed-claude-ide-server",
    version,
    about = "Companion process that lets the claude CLI see what is selected in Zed"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Log at debug level. RUST_LOG=debug does the same.
    #[arg(long, short, global = true)]
    debug: bool,

    /// Project root. Defaults to the working directory.
    #[arg(long, global = true)]
    worktree: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve Zed over LSP on stdio and the claude CLI over a loopback WebSocket.
    ///
    /// This is what the extension launches, and the default when no subcommand
    /// is given. `hybrid` is accepted as an alias for extension builds that
    /// still pass it.
    #[command(alias = "hybrid")]
    Serve {
        /// Bind this port instead of a random one from the protocol's range.
        #[arg(long, short)]
        port: Option<u16>,
    },
    /// Mention the current Zed selection in the connected claude CLI.
    ///
    /// Meant to be bound to a key through a Zed task; see docs/at-mentions.md.
    AtMention,
}

/// How often to check whether Zed is still our parent.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.debug)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run(cli));

    // stdin is read on a blocking thread that cannot be cancelled, and dropping a
    // runtime waits for its blocking threads. When we stop for a reason other
    // than stdin closing -- a signal, Zed dying -- that read is still parked, so
    // an unbounded wait here would never return. The lock file is already gone
    // by this point; nothing of value is lost by giving up on the thread.
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

async fn run(cli: Cli) -> Result<()> {
    // The only place the environment is consulted. Everything below is handed
    // the answers.
    let worktree = match cli.worktree {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let lock_dir = LockDir::resolve(
        std::env::var_os("ZED_CLAUDE_IDE_DIR"),
        std::env::var_os("CLAUDE_CONFIG_DIR"),
    )?;

    match cli.command.unwrap_or(Command::Serve { port: None }) {
        Command::Serve { port } => serve(Config::new(worktree, lock_dir).with_port(port)).await,
        Command::AtMention => {
            let lock = discovery::lock_for(&lock_dir, &worktree)?;
            at_mention::send_at_mention_request(&lock).await
        }
    }
}

fn init_tracing(debug: bool) -> Result<()> {
    let level = if debug {
        tracing::Level::DEBUG
    } else {
        match std::env::var("RUST_LOG").as_deref() {
            Ok("trace") => tracing::Level::TRACE,
            Ok("debug") => tracing::Level::DEBUG,
            Ok("warn") => tracing::Level::WARN,
            Ok("error") => tracing::Level::ERROR,
            _ => tracing::Level::INFO,
        }
    };

    let subscriber = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_target(false)
        // stdout is the LSP channel; everything else goes to stderr.
        .with_writer(std::io::stderr)
        // Zed captures stderr into its LSP Logs panel, which renders escape
        // sequences literally -- colour codes there are noise, not formatting.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;
    Ok(())
}

/// Run the companion until any of the reasons to stop arrives, then take the one
/// shutdown path: stop accepting, close clients, remove the lock file.
async fn serve(config: Config) -> Result<()> {
    info!(
        "zed-claude-ide-server {} starting for {}",
        env!("CARGO_PKG_VERSION"),
        config.worktree.display()
    );
    let handle = Companion::start(config).await?;

    let reason = tokio::select! {
        // Zed closed our stdin: the normal end of a session.
        _ = handle.run_lsp(tokio::io::stdin(), tokio::io::stdout()) => "Zed closed the LSP channel",
        // Zed died without closing anything.
        _ = lsp::parent_exited(WATCHDOG_INTERVAL) => "parent process exited",
        // Someone asked. Zed sends SIGTERM on `editor: restart language server`.
        signal = termination_signal() => signal,
        // The accept loop gave up on its listener.
        _ = handle.stopped() => "the WebSocket server stopped",
    };
    info!("{}; shutting down", reason);
    handle.shutdown().await;
    Ok(())
}

async fn termination_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = terminate.recv() => "SIGTERM",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "ctrl-c"
    }
}
