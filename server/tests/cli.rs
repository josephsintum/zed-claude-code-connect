//! The command-line surface the extension and the at-mention task depend on,
//! exercised against the built binary.
//!
//! The extension launches `<binary> --worktree <root> serve`; older extension
//! builds still pass `--debug --worktree <root> hybrid`, so that must keep
//! parsing. There is no other caller of any other mode.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_claude-code-connect"))
}

/// Run the companion with stdin already at EOF, so the LSP side ends at once and
/// the process exits on its own. Returns the exit status and whether any lock
/// file was left behind in `ide_dir`.
fn run_to_exit(args: &[&str], ide_dir: &std::path::Path) -> (std::process::ExitStatus, bool) {
    let mut child = bin()
        .args(args)
        .env("ZED_CLAUDE_IDE_DIR", ide_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    drop(child.stdin.take()); // EOF

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "process did not exit after stdin EOF"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let leftover = std::fs::read_dir(ide_dir)
        .map(|d| {
            d.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "lock"))
        })
        .unwrap_or(false);
    (status, leftover)
}

#[test]
fn help_lists_serve_and_at_mention_only() {
    let out = bin().arg("--help").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let commands: Vec<&str> = text
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        // clap's own; not part of this program's surface.
        .filter(|c| *c != "help")
        .collect();
    assert_eq!(
        commands,
        vec!["serve", "at-mention"],
        "the public surface is exactly these two; got:\n{text}"
    );
}

#[test]
fn serve_with_global_worktree_before_the_subcommand_parses_and_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let wt = dir.path().to_str().unwrap();
    let (status, leftover) = run_to_exit(&["--worktree", wt, "serve"], dir.path());
    assert!(status.success(), "serve exited with {status}");
    assert!(!leftover, "stdin EOF must remove the lock file");
}

#[test]
fn the_old_extension_invocation_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let wt = dir.path().to_str().unwrap();
    let (status, leftover) = run_to_exit(&["--debug", "--worktree", wt, "hybrid"], dir.path());
    assert!(status.success(), "hybrid alias exited with {status}");
    assert!(!leftover);
}

#[test]
fn no_subcommand_means_serve() {
    let dir = tempfile::tempdir().unwrap();
    let wt = dir.path().to_str().unwrap();
    let (status, leftover) = run_to_exit(&["--worktree", wt], dir.path());
    assert!(status.success());
    assert!(!leftover);
}

#[test]
fn the_removed_modes_are_rejected() {
    for mode in ["lsp", "websocket"] {
        let out = bin().arg(mode).stdin(Stdio::null()).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "`{mode}` must be a usage error, not a thing that runs"
        );
        let mut err = String::new();
        std::io::Cursor::new(out.stderr)
            .read_to_string(&mut err)
            .unwrap();
        assert!(err.contains("unrecognized subcommand"), "{err}");
    }
}
