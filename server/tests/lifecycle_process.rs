//! How the companion process ends, exercised against the built binary.
//!
//! Every way the process can stop must remove its lock file, or the CLI keeps
//! dialling a port nobody answers until it happens to notice the pid is dead.
//! Zed ends the companion by closing stdin on a normal quit, by SIGTERM on a
//! language-server restart, and by simply vanishing on a crash, which leaves the
//! companion reparented.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed-claude-ide-server"))
}

fn lock_in(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "lock"))
}

fn wait_for_lock(dir: &Path) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(p) = lock_in(dir) {
            return p;
        }
        assert!(
            Instant::now() < deadline,
            "companion never wrote a lock file"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn wait_for_exit(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Start `serve` with stdin held open by the test, as Zed holds it.
fn spawn_serve(dir: &Path) -> Child {
    Command::new(bin())
        .args(["serve", "--worktree", dir.to_str().unwrap()])
        .env("ZED_CLAUDE_IDE_DIR", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn companion")
}

fn signal_removes_the_lock(signal: i32, name: &str) {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_serve(dir.path());
    let lock = wait_for_lock(dir.path());

    unsafe {
        libc::kill(child.id() as i32, signal);
    }
    let exited = child
        .wait_timeout(Duration::from_secs(3))
        .unwrap_or_else(|| panic!("process did not exit within 3s of {name}"));
    let _ = exited;
    assert!(
        !lock.exists(),
        "{name} must remove the lock file; {} survived",
        lock.display()
    );
}

trait WaitTimeout {
    fn wait_timeout(&mut self, within: Duration) -> Option<std::process::ExitStatus>;
}

impl WaitTimeout for Child {
    fn wait_timeout(&mut self, within: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(s) = self.try_wait().unwrap() {
                return Some(s);
            }
            if Instant::now() >= deadline {
                let _ = self.kill();
                let _ = self.wait();
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn sigint_removes_the_lock_and_exits_promptly() {
    signal_removes_the_lock(libc::SIGINT, "SIGINT");
}

/// What Zed sends on `editor: restart language server`. Observed live: the old
/// companion's lock survived every restart.
#[test]
fn sigterm_removes_the_lock_and_exits_promptly() {
    signal_removes_the_lock(libc::SIGTERM, "SIGTERM");
}

#[test]
fn stdin_eof_removes_the_lock_and_exits_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_serve(dir.path());
    let lock = wait_for_lock(dir.path());

    drop(child.stdin.take());
    child
        .wait_timeout(Duration::from_secs(3))
        .expect("process did not exit within 3s of stdin EOF");
    assert!(!lock.exists(), "stdin EOF must remove the lock file");
}

/// Zed dies without closing anything: the companion is reparented. The watchdog
/// polls for that every five seconds and must take the lock file with it.
#[test]
fn losing_the_parent_removes_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    // A short-lived shell starts the companion in the background, reports its
    // pid, and exits, leaving the companion an orphan. stdin is the test's pipe,
    // kept open, so the LSP side does not see EOF and cannot be the reason it
    // stops. The companion's own stdio is redirected so the shell's stdout carries
    // only the pid.
    let mut sh = Command::new("sh")
        .arg("-c")
        // A background job in a non-interactive shell gets /dev/null as stdin,
        // which would end the companion at once; hand it the real one via fd 3.
        .arg(r#"exec 3<&0; "$0" serve --worktree "$1" <&3 >/dev/null 2>&1 & echo $!; sleep 0.2"#)
        .arg(bin())
        .arg(dir.path())
        .env("ZED_CLAUDE_IDE_DIR", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = sh.stdin.take().unwrap();
    let out = {
        let mut s = String::new();
        std::io::Read::read_to_string(sh.stdout.as_mut().unwrap(), &mut s).unwrap();
        s
    };
    let _ = sh.wait();
    let pid: u32 = out.trim().parse().expect("shell printed the companion pid");
    // Keep the pipe open for the whole test so EOF cannot be the trigger.
    let _ = stdin.write_all(b"");

    let lock = wait_for_lock(dir.path());
    assert!(
        alive(pid),
        "companion should be running once its lock exists"
    );

    assert!(
        wait_for_exit(pid, Duration::from_secs(12)),
        "orphaned companion did not exit"
    );
    assert!(
        !lock.exists(),
        "an orphaned companion must remove its lock; {} survived",
        lock.display()
    );
    drop(stdin);
}

/// How Zed actually ends a language server on quit or restart: the LSP
/// `shutdown` request, the `exit` notification, then it kills the process --
/// without closing stdin first and without SIGTERM. Observed live: every restart
/// left the previous companion's lock behind.
#[test]
fn lsp_shutdown_and_exit_remove_the_lock_and_end_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_serve(dir.path());
    let lock = wait_for_lock(dir.path());

    let mut stdin = child.stdin.take().unwrap();
    let frame = |body: &str| format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    stdin
        .write_all(
            frame(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#)
                .as_bytes(),
        )
        .unwrap();
    stdin
        .write_all(frame(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#).as_bytes())
        .unwrap();
    stdin
        .write_all(
            frame(r#"{"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}"#).as_bytes(),
        )
        .unwrap();
    stdin
        .write_all(frame(r#"{"jsonrpc":"2.0","method":"exit"}"#).as_bytes())
        .unwrap();
    stdin.flush().unwrap();
    // stdin stays open: Zed does not close it, it kills the process.

    child
        .wait_timeout(Duration::from_secs(3))
        .expect("process did not exit within 3s of the LSP exit notification");
    assert!(
        !lock.exists(),
        "the LSP exit must remove the lock; {} survived",
        lock.display()
    );
    drop(stdin);
}
