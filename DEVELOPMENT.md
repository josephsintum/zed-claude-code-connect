# Development

For what the project is and how it works, see [docs/architecture.md](docs/architecture.md).

## Layout

```
extension/                 the Zed extension (Rust -> wasm32-wasip2)
  src/lib.rs                    resolves the companion binary, returns the spawn command
  extension.toml                extension id, and the languages that trigger activation

server/          the native companion
  src/main.rs                   `serve` (what the extension launches) and `at-mention`;
                                the only place the environment is read; the one shutdown path
  src/companion.rs              Companion::start(Config) -> Handle; the handle owns the lock file
  src/config.rs                 what a companion is told at startup: worktree, lock dir, port, debounce
  src/lockfile.rs               the lock-file contract, atomic write, and the guard that removes it
  src/discovery.rs              finding a running companion, as the CLI does
  src/selection.rs              Selection, the debouncing tracker, and the event bus
  src/at_mention.rs             the hotkey client
  src/lsp/
    server.rs                   the LSP server; selection is harvested in code_action
    documents.rs                in-memory mirror of open buffers
    watchdog.rs                 notices when Zed dies without closing stdin
  src/mcp/
    server.rs                   port binding and the accept loop
    connection.rs               one CLI connection: auth, dispatch, event forwarding
    protocol.rs                 MCP envelope types, dispatcher, tools
    wire.rs                     the wire shapes of selection_changed / at_mentioned
  examples/watch.rs             connects as the CLI does and prints notifications
  tests/                        fake CLI + fake Zed rig (common/), handshake, editor path,
                                lock file, discovery, CLI surface, process exits

resolve/         pure binary-resolution rules the extension uses, testable on the host
```

## Prerequisites

```sh
rustup target add wasm32-wasip2
```

## The loop

**Changing the companion** — the common case:

```sh
cargo build --release -p claude-code-ide-server
```

Then `editor: restart language server` in Zed. No extension reinstall, no Zed
restart. Confirm it took effect in Zed's **LSP Logs** panel (not `Zed.log` — that
only carries Zed's own messages about the server, such as protocol errors).

**Changing the extension** — rare:

```sh
cargo build --release -p zed-claude-ide --target wasm32-wasip2
```

Then `zed: install dev extension` again and re-select the `extension/`
directory. Zed compiles the WASM itself.

**First-time install**: `zed: install dev extension`, select `extension/`,
then point Zed at your local build in `~/.config/zed/settings.json`:

```json
"lsp": {
  "claude-code-ide-server": {
    "binary": { "path": "/abs/path/to/target/release/claude-code-ide-server" }
  }
}
```

Without that, the extension tries to download a release binary from `GITHUB_REPO`
in `extension/src/lib.rs`.

## Checks

```sh
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI runs all three plus a WASM build of the extension. Zed compiles the extension
itself, so a break there would otherwise only appear when a user installs it.

## Seeing what is on the wire

```sh
cargo run -p claude-code-ide-server --example watch                  # list companions
cargo run -p claude-code-ide-server --example watch -- /path/to/proj # attach
```

This connects exactly as the CLI does and prints each notification with timestamps
and inter-event gaps. It is the fastest way to tell whether selection is flowing,
and it can run alongside a real `claude` — both receive the broadcast.

## Environment

| Variable | Use |
| --- | --- |
| `ZED_CLAUDE_IDE_DIR` | Override the lock directory. |
| `CLAUDE_CONFIG_DIR` | Honoured when the above is unset: locks go to `$CLAUDE_CONFIG_DIR/ide`, which the CLI scans alongside `~/.claude/ide`. |
| `CLAUDE_CODE_AUTO_CONNECT_IDE=true` | Makes `claude` attach without `/ide`. |
| `CLAUDE_CODE_IDE_SKIP_VALID_CHECK=true` | Makes the CLI accept any lock regardless of cwd. Debugging only — it is the only thing preventing attachment to another project's companion. |

Do not set `FORCE_CODE_TERMINAL`. It makes the CLI believe it is inside an IDE's own
terminal, enabling a check that the lock's `pid` be an ancestor of the CLI process —
never true from a separate terminal.

## Tests

`tests/handshake.rs` is a fake CLI performing the real handshake; `tests/editor.rs`
drives a fake Zed over LSP and asserts on the CLI side, using the rig in
`tests/common/`. When adding to either:

- **Revert the fix and confirm the test fails.** A test that passes against the
  defect it was written for is worth nothing, and that has already happened here
  once.
- **Assert absence precisely.** `serde_json` yields `Value::Null` for a missing key,
  so `is_null()` cannot tell "omitted" from "explicitly null" — a distinction this
  protocol cares about. Use `contains_key`.
