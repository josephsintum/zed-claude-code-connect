# Companion refactor: typed core, two adapters, one lifecycle

Design record for the staged refactor of `claude-code-connect` carried out on
2026-09-14. The architecture in [docs/architecture.md](../../architecture.md) is
unchanged; this records why the code was restructured, what was decided, and how
each stage was verified.

## Why

Fourteen commits in one day took an imported fork from "does not work with the
real CLI" to "works and is documented". The protocol corrections were the asset;
the structure had grown around them:

- Selection state lived in three places: a JSON cache in the WebSocket module, a
  per-connection copy in the MCP server, and the debouncer's serialised string.
- The internal event bus carried `method: String, params: Value`; consumers
  re-parsed JSON to recover values typed two functions earlier.
- The server entry point had accreted to five optional arguments through four
  nested wrappers; tests called the innermost and polled for the lock file.
- The library read environment variables itself, so every test set a
  process-global variable under a mutex.
- The lock file was created in one place and removed in three, and the watchdog's
  `process::exit` removed it in none. SIGTERM, which Zed sends on a language-server
  restart, had no handler at all. Two dead-pid locks were observed in one session.
- The extension's PATH tier could never work (Zed joins a relative command onto
  its work directory), every start hit the GitHub API, and an interrupted download
  could be run as the server next start.

## Decisions

- Staged, behaviour-preserving refactor; not a rewrite. The fake-CLI harness is
  the safety net; a fake-Zed rig was added first because nothing tested the
  editor side.
- Sequence: gate experiment, then fixes on the current structure, then structure.
  The gate was whether Zed keeps issuing `codeAction` once a server returns an
  empty list. Measured live: it does.
- Bundled behaviour changes: no code actions and no execute-command capability;
  the CLI collapses to `serve` (hidden alias `hybrid`) and `at-mention`; info
  logging by default.
- `LockFile` (strict, written) and `RawLock` (lenient, read from a directory other
  editors also write into) stay two types, co-located, as a deliberate
  trust-boundary asymmetry.
- The lock-file leak family is solved once by an RAII guard owned by the
  companion's `Handle`, not patched per exit path.
- `GITHUB_REPO` remains a placeholder; it is an open release blocker until the
  repository exists.

## Shape

```
main.rs         parse args; read the environment once; one select over every
                reason to stop; Handle::shutdown; bounded runtime teardown
companion.rs    Companion::start(Config) -> Handle { port, token, LockGuard,
                CancellationToken, tasks, EventBus, SelectionTracker }
config.rs       worktree, lock dir, port, debounce
lockfile.rs     LockFile / RawLock, LockDir (resolve, prepare, write, remove), LockGuard
discovery.rs    all_locks(&LockDir), lock_for(&LockDir, path)
selection.rs    Selection, Mention, Event, EventBus (latest + broadcast), SelectionTracker
at_mention.rs   the hotkey client
lsp/            server.rs (LanguageServer impl, serve_lsp), documents.rs, watchdog.rs
mcp/            server.rs (bind, accept), connection.rs, protocol.rs, wire.rs
```

Dependency direction: `main -> {companion, config, discovery, at_mention}`;
`companion -> {lockfile, selection, lsp, mcp}`; `lsp -> selection`;
`mcp -> selection`; `discovery -> lockfile`. Only `main.rs` and
`examples/watch.rs` read the environment.

## Lifecycle

`Companion::start` binds, writes the lock, and spawns the tracker and the accept
loop under one `CancellationToken`. `main` selects over: LSP input ended, parent
process gone, SIGINT, SIGTERM, accept loop stopped. Then `Handle::shutdown`:
cancel, each connection sends a 1001 close frame, tasks are joined with a 2 s
bound, the guard drops and removes the lock. Because `process::exit` is gone and
tokio's stdin read cannot be cancelled, `main` builds the runtime itself and calls
`shutdown_timeout` so a still-open pipe cannot hang the exit.

## Testing

Three layers, each written before the code it protects and run against the
previous code first:

- Unit: the tracker under paused time; the lock guard; the resolution rules the
  extension uses (in a dependency-free crate, since the extension is WASM-only).
- In-process: a fake Zed over an in-memory pipe and a fake CLI on the socket,
  together in `tests/editor.rs`; the handshake suite; lock-file and discovery
  contracts; the `Handle` lifecycle.
- Process: the built binary's command-line surface, and every exit route
  (SIGINT, SIGTERM, stdin EOF, orphaned) removing the lock within a bound.

Every stage ended with `cargo fmt --check`, `cargo clippy --locked --all-targets
-- -D warnings`, `cargo test --locked --workspace`, the WASM build, and a live
attach with `examples/watch` after a language-server restart in Zed.

## Deliberately not done

Diagnostics, diff review, a machine-wide daemon, an alternative selection source
(structural limits, see architecture.md §11); Windows; the 4 MiB mirror cap; the
80 ms flush in the at-mention client.
