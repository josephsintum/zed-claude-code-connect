# How the protocol was determined

Claude Code's IDE integration is not publicly specified. Everything in
[docs/architecture.md §4](docs/architecture.md#4-the-protocol) was reconstructed
from two shipped artefacts, both version 2.1.269:

- `~/.vscode/extensions/anthropic.claude-code-<version>/extension.js` — the VS Code
  extension, bundled and minified but readable. This is the reference
  implementation of the server side, which is the side we implement.
- The `claude` binary itself — a compiled Bun executable whose JavaScript is
  embedded as plain text and greppable.

Reading both matters. The extension shows what a working server sends; the CLI
shows what it actually *accepts*, and the two are not the same. Examples that only
the CLI reveals:

- `diagnostics_changed` is sent by the VS Code extension, but the string does not
  appear anywhere in the CLI. It is ignored; the CLI polls `getDiagnostics` instead.
- The CLI hides every IDE tool from the model except `executeCode` and
  `getDiagnostics`.
- `ENABLE_IDE_INTEGRATION`, which older write-ups mention, does not exist in 2.1.269.
- The port is read from the lock file's *name*; a port field in the JSON is ignored.
- The CLI never unlinks a lock file, and checks the lock's `pid` for liveness only
  when `TERM_PROGRAM` says it is inside a VS Code or JetBrains terminal (or
  `FORCE_CODE_TERMINAL` is set). From any other terminal, every lock whose folder
  matches the cwd is listed in `/ide`, dead or alive. The VS Code extension removes
  its own lock on `deactivate` (raced against a five-second timeout) and never
  sweeps others; it writes `pid: process.ppid`, the editor process, not its own.

## Re-verifying after a CLI update

The protocol has been stable across patch versions, but if something breaks after
an update, these are the load-bearing strings to grep for in the binary:

```sh
CLI=$(readlink -f "$(which claude)")
strings "$CLI" | grep -E "selection_changed|at_mentioned|ide_connected"
strings "$CLI" | grep -E "x-claude-code-ide-authorization|\.claude/ide"
strings "$CLI" | grep -E "CLAUDE_CODE_AUTO_CONNECT_IDE|CLAUDE_CODE_SSE_PORT"
```

## Watching live traffic

Reading the implementations tells you what *should* happen. To see what does:

```sh
cargo run -p zed-claude-ide-server --example watch -- /path/to/project
```

This connects to a running companion exactly as the CLI does and prints every
notification. It can run alongside a real `claude` session — both clients receive
the broadcast — so you can watch what the CLI is being told while using it.

For the raw frames, `tcpdump` on loopback against the port named by the lock file
works, though the WebSocket framing makes it harder to read than the above:

```sh
sudo tcpdump -i lo0 -A -tttt port <port>
```
