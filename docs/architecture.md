# Architecture

How this project is put together, and why it is shaped the way it is.

For installation and day-to-day use see [the README](../README.md). For the
`@`-mention hotkey see [at-mentions.md](at-mentions.md).

---

## 1. The problem

When you run `claude` in a terminal, it has no idea what you are looking at. You
end up pasting paths and line ranges by hand, or describing code the tool could
simply read.

In VS Code, the Claude Code extension closes that gap: the CLI knows your active
file, tracks your selection as it changes, and a hotkey drops `@path#L12-20` into
the prompt. This project provides the same thing for Zed — including when `claude`
runs in a **separate terminal** such as Ghostty or iTerm, not Zed's built-in one.

## 2. Why this is not just an extension

The obvious design — a Zed extension that watches the editor and talks to the CLI —
is impossible. Two independent blocks, either of which alone is fatal.

**A Zed extension cannot open a socket.** Extensions compile to WebAssembly and run
under WASI. Zed's extension host builds the WASI context with stdio, two preopened
directories and a couple of environment variables, and never links `wasi:sockets`.
There is no `bind()` or `listen()` reachable from extension code at any privilege
level. The only egress is Zed's own HTTP client, which does request/response and
cannot accept connections.

**A Zed extension cannot see the editor.** The extension API has no buffer, pane,
editor, cursor or selection type anywhere in it. `Project` — the handle an extension
receives — exposes exactly one method, `worktree_ids()`. There is no event or
subscription channel either: an extension is a set of functions Zed *calls*, not a
subscriber to what the user is doing.

Zed maintainers have acknowledged this class of integration as something that
"should be doable with an extension in the future (but isn't right now)"
([zed#58338](https://github.com/zed-industries/zed/discussions/58338)). If that
lands, most of this project becomes deletable.

**The way around both** is to have the extension do almost nothing except ask Zed to
spawn a **native companion process**, registered as a language server. Being native,
the companion can bind a port. Being a language server, it receives editor state.

## 3. The pieces

```
┌───────────────────────── Zed ─────────────────────────┐
│                                                        │
│   zed-claude-ide  (WASM, ~380 lines)                   │
│     tells Zed what to spawn, and fetches the binary    │
│                                                        │
└──────────────────────────┬─────────────────────────────┘
                           │ spawns, then speaks LSP over stdio
                           ▼
              ┌────────────────────────────┐
              │  zed-claude-ide-server     │   one per Zed window
              │  (native, ~2600 lines)     │
              │                            │
              │  • LSP server  (from Zed)  │
              │  • MCP server  (to claude) │
              │  • buffer mirror           │
              │  • lock file               │
              └─────────┬──────────────────┘
                        │ WebSocket, MCP JSON-RPC 2.0
                        ▼
              ┌────────────────────────┐
              │  claude  (any terminal)│
              └────────────────────────┘
```

### `zed-claude-ide` — the extension crate

A `cdylib` compiled to `wasm32-wasip2`. Its whole job is `language_server_command`:
return a path to the companion binary and the arguments to run it with. It resolves
the binary in three tiers:

1. An explicit path in Zed's `lsp` settings — the development loop.
2. A GitHub release asset, downloaded on first use and cached under a
   version-named path in the extension's work directory. The resolved path is
   remembered for the session, so the GitHub API is asked once, not once per
   worktree.
3. `zed-claude-ide-server` from `PATH`, resolved to an absolute path with
   `worktree.which`. A bare name cannot work here: Zed joins a relative command
   onto the extension's work directory rather than searching `PATH`.

Installing the extension does **not** install the server. Zed installs only the
`.wasm` and `extension.toml`; the binary arrives lazily the first time a matching
file is opened. This is the standard pattern for Zed language-server extensions.

### `zed-claude-ide-server` — the companion crate

A native binary with two subcommands. `serve` runs the LSP server on stdio and
the MCP server on a loopback socket, and is what the extension launches (`hybrid`
is accepted as an alias for older extension builds). `at-mention` acts as a client
rather than a server — see §6.2.

| Module | Role |
| --- | --- |
| `companion.rs` | The companion as a value: `Companion::start(Config)` returns a `Handle` that owns the lock file and stops everything when dropped |
| `config.rs` | What a companion is told at startup; `main` is the only reader of the environment |
| `lockfile.rs` | The lock-file contract, its atomic owner-only write, and the guard that removes it |
| `discovery.rs` | Finding a running companion the way the CLI does |
| `selection.rs` | The `Selection` type, the debouncing tracker the editor side feeds, and the event bus every CLI connection subscribes to |
| `lsp/server.rs` | The LSP server Zed talks to; where selection is harvested from `codeAction` |
| `lsp/documents.rs` | In-memory mirror of open buffers |
| `lsp/watchdog.rs` | Notices when Zed has died without closing stdin |
| `mcp/server.rs` | Binding the port and accepting connections |
| `mcp/connection.rs` | One CLI connection: auth, request dispatch, event forwarding |
| `mcp/protocol.rs` | MCP envelope types, the request dispatcher, and the tools |
| `mcp/wire.rs` | The one place `selection_changed`, `at_mentioned` and the selection tool payloads are built |
| `at_mention.rs` | The hotkey client that asks a running companion to mention its selection |

## 4. The protocol

This part is not documented publicly. It was reconstructed by reading the shipped
VS Code extension (`extension.js`) and the `claude` binary, both v2.1.269.

**The surprise is the direction: the editor is the server and the CLI is the
client.** Most people assume the reverse.

### 4.1 Discovery

1. The companion binds `127.0.0.1` on a random free port in `[10000, 65535]`.
2. It writes `~/.claude/ide/<port>.lock`, file mode `0600`, directory mode `0700`:

   ```json
   {"pid": 1234,
    "workspaceFolders": ["/Users/you/project"],
    "ideName": "Zed",
    "transport": "ws",
    "runningInWindows": false,
    "authToken": "3d6043a4-1b89-44bf-bc77-e4bd7a577948"}
   ```

   **The port comes from the filename, not the JSON.** The CLI parses it out of
   `<port>.lock` and ignores any port field in the body.

3. `claude` scans that directory and accepts a lock when its working directory is
   the workspace folder or lies beneath it. It checks that `pid` names a live
   process **only when it believes it is running inside an IDE's own terminal**
   (`TERM_PROGRAM` is VS Code or JetBrains, or `FORCE_CODE_TERMINAL` is set). From
   any other terminal there is no liveness check, and the CLI never unlinks a
   lock: a lock left by a dead companion is listed in `/ide` as a live "Zed" entry
   until something else removes it. That is why the companion sweeps its own dead
   locks at startup (§7).

Because discovery is entirely file-based, the CLI does not need to be a child of
the editor, which is what makes a separate terminal work at all. Nothing can inject
environment variables into Ghostty from Zed, so the `CLAUDE_CODE_SSE_PORT` shortcut
the VS Code extension uses is unavailable here — and unnecessary.

### 4.2 Connection

The CLI dials `ws://127.0.0.1:<port>` — no path, no query — offering the WebSocket
subprotocol `mcp` and the header `X-Claude-Code-Ide-Authorization: <authToken>`.
The server must echo `Sec-WebSocket-Protocol: mcp` or negotiation fails.

A wrong or missing token is rejected by completing the upgrade and then closing
with code **1008**. Rejecting at the HTTP layer with a 401 does not work: the close
frame only exists after the handshake.

Then it is plain MCP JSON-RPC 2.0, one JSON object per WebSocket text frame. No
batching, no length prefix. The CLI sends `initialize`, then the notification
`ide_connected {pid}`, then `tools/list`.

`ide_connected` is the one notification that does not start with `notifications/`,
which is a small trap: a guard testing for that prefix lets it through to the
request path and answers it with `-32601`, which is a JSON-RPC violation.

### 4.3 What actually carries the context

The MCP *tools* are mostly incidental. The feature is two push notifications that
travel editor → CLI:

**`selection_changed`** — the live selection.

```json
{"text": "const x = 1;",
 "filePath": "/Users/you/project/src/a.ts",
 "selection": {"start": {"line": 11, "character": 0},
               "end": {"line": 19, "character": 8},
               "isEmpty": false}}
```

**`at_mentioned`** — inserts `@path#L12-20` into the CLI prompt.

```json
{"filePath": "/Users/you/project/src/a.ts", "lineStart": 11, "lineEnd": 19}
```

Lines are **0-based**; the CLI adds one for display. The line fields are optional
and must be **omitted** — not sent as `null` — when there is no selection, which
the CLI reads as "the whole file". Its validator accepts a missing key but rejects
null.

### 4.4 Tools

The CLI filters the IDE's tool list before the model ever sees it, leaving only
`mcp__ide__executeCode` and `mcp__ide__getDiagnostics`. This server advertises
neither: there is no Jupyter kernel, and diagnostics are unreachable (§8).

What it does advertise, all real:

| Tool | Returns |
| --- | --- |
| `getCurrentSelection` | The tracked selection |
| `getLatestSelection` | The last selection, even if the editor lost focus |
| `getWorkspaceFolders` | The worktree root |

Tools the CLI may call internally — `openDiff`, `openFile`, `saveDocument`,
`close_tab` and friends — are answered with an explicit "not supported" rather than
advertised or faked. That matters: an earlier project in this lineage returned
`FILE_SAVED` unconditionally from `openDiff`, which tells the CLI a human reviewed
and accepted an edit. Every change would have auto-approved with no review.

## 5. Getting editor state out of Zed

LSP has no "selection changed" notification. It has document sync, and requests
that carry a position. So how does live selection work at all?

**Zed issues `textDocument/codeAction` with the live selection range whenever the
cursor or selection moves** — it needs to know whether to show the code-action
indicator. The companion piggybacks on that.

This was measured, not assumed. Attaching to a running companion as a second
client and moving around in Zed produces, among others:

```
selection_changed  157:0-157:0      0 chars   ""                       ← cursor move
selection_changed  132:15-150:10  878 chars   "if (typeof message..."  ← selection
selection_changed   81:2-86:69    497 chars   "  // The capture tag..."
selection_changed   81:2-85:69    420 chars   (+894ms)                 ← shrinking
selection_changed   81:2-83:69    242 chars   (+564ms)                 ← with shift-arrow
```

Both bare cursor moves and real selections arrive, at sub-second latency, with no
explicit `cmd-.`. You can reproduce this with `cargo run --example watch`.

**This is the load-bearing assumption of the whole design.** If Zed stopped
requesting code actions on selection change, live tracking would stop with it and
the `@`-mention hotkey would become the only path.

The cost of this route is that registration goes through `language_server_command`,
so the companion only starts when a file of a *listed* language is opened.
`extension.toml` lists 44 languages including `"Plain Text"` to make that fire as
early as possible.

## 6. Data flows

### 6.1 A selection

```
user selects text in Zed
   │
   ├─> Zed: textDocument/codeAction { range }
   │        │
   │        ├─ read the range from the buffer mirror (not from disk)
   │        └─ SelectionTracker::update(Selection)
   │                 └─ 300ms debounce, drop if identical to the last one sent
   │                      │
   │                      └─> EventBus::publish_selection
   │                               ├─> recorded as `latest`
   │                               └─> every connected WebSocket client
   │                                        │
   └────────────────────────────────────────┴─> claude, as selection_changed
                                                (built in mcp/wire.rs)
```

Two details worth their weight:

*Debouncing.* Zed asks for code actions on every cursor move, so holding
shift-arrow through a block produces a burst, each carrying the full selected text.
Bursts are coalesced on a 300ms timer and identical consecutive payloads dropped —
matching what the VS Code extension does.

*The buffer mirror.* Selection text comes from an in-memory copy of the buffer
maintained from `didOpen`/`didChange`, not from the file on disk. Reading from disk
returns pre-edit text for any unsaved buffer — which is the normal state while you
are actually working, and exactly when the context matters most.

### 6.2 An at-mention

The hotkey cannot call the companion directly, because Zed extensions cannot
register commands or keybindings. A Zed **task** can be bound to a key and can run
a command, so that is the route:

```
keypress
   └─> Zed task: `zed-claude-ide-server at-mention --worktree $ZED_WORKTREE_ROOT`
          │
          ├─ scan ~/.claude/ide/*.lock for our live companion covering that path
          ├─ connect with the token from that lock
          └─ send at_mention_request  (no range)
                 │
          companion: turn the tracked selection into at_mentioned
                 └─> claude
```

The request deliberately carries **no line range**. The companion already tracks the
live selection, so sending coordinates would be redundant — and worse, unreliable:
Zed exposes `ZED_ROW` but nothing for the *other* end of a selection, so a task
passing coordinates could not tell whether the row it was handed is the anchor or
the head.

Reusing the existing WebSocket rather than adding a Unix socket means discovery,
authentication and transport are already solved and have one implementation.

### 6.3 Connecting mid-session

A client that attaches after you have already selected something has missed every
notification sent up to that point. Without a replay it would know nothing until
your next cursor move.

The most recent `selection_changed` is therefore cached independently of any
connection, and replayed to each new client on connect. The VS Code extension does
the same thing about 500ms after connect.

## 7. Process and lifecycle

**One companion per Zed window**, since Zed runs one language server instance per
worktree. Four open projects means four companions, four ports, four lock files,
four tokens. A `claude` in any project directory finds its own.

```
Zed window opens a project
   └─ first matching file opened
        └─ Zed spawns the companion
             ├─ binds a random free port
             ├─ writes ~/.claude/ide/<port>.lock
             └─ serves LSP on stdio + MCP on the socket
                  ...
Zed window closes
   └─ companion's stdin reaches EOF, LSP loop ends, lock file removed
```

Every way the process can stop goes through one path. `main` waits for whichever
comes first — Zed closing stdin, the parent process disappearing (polled every
five seconds), SIGINT, SIGTERM (which Zed sends on `editor: restart language
server`), or the accept loop giving up on its listener — then calls
`Handle::shutdown`: stop accepting, close each connected CLI with a 1001 frame,
and drop the guard that owns the lock file. A handle dropped any other way, by a
`?` or a panic, removes the lock the same way.

Zed does not use any of those. On quit and on `editor: restart language server` it
sends the LSP `shutdown` request, then the `exit` notification, then kills the
process without closing stdin. The companion therefore treats `exit` as a reason to
stop and removes the lock before anything else, since the kill is racing it. When it
loses that race, the next companion to start sweeps every lock that carries our
`ideName` and names a dead pid. Not a stranger's lock, and not a live one, which
belongs to another window. The CLI itself never removes a lock (§4.1).

One consequence of not calling `process::exit`: tokio reads stdin on a blocking
thread that cannot be cancelled, and dropping the runtime waits for it. When the
reason to stop is anything but stdin closing, Zed still holds the pipe, so `main`
bounds the teardown with `shutdown_timeout`. The lock is gone by then.

**Other editors write to the same directory.** VS Code's Claude extension uses
`~/.claude/ide/` too, so with both editors open on one project there will be two
locks for that path. Ours is identified by `"ideName": "Zed"`; discovery filters on
it. Skipping that check means the at-mention silently goes to VS Code.

## 8. Design decisions

**`language_server_command`, not `context_server_command`.** Zed can also register
MCP context servers, which looks like a closer fit for a thing that speaks MCP. It
is the wrong choice twice over: a context server is an MCP server for *Zed's own*
agent and receives no editor events, so live selection would be lost entirely; and
`Project` cannot even tell you the worktree root, which the lock file needs.
Registering both would spawn two companions per project and put two identical "Zed"
entries in the CLI's `/ide` picker, one of them deaf.

**The companion is invisible as a language server.** It attaches to every buffer,
so anything it advertises is a request Zed routes to it instead of, or alongside,
the project's real language server. It therefore declares only document sync and
code actions, and answers every code-action request with an empty list — measured
live, Zed keeps asking. An earlier version claimed `definition`, `references`,
`documentSymbol` and `workspaceSymbol` with no handler behind any of them — Zed's
edit-prediction filled the log with `-32601` on every keystroke — and answered every
completion request with three `@claude ...` items that appeared in every popup in
every file.

**Diagnostics are not offered.** The companion is one language server among many,
and Zed does not forward other servers' `publishDiagnostics` to it. The tool could
only ever answer "no problems found". Since the CLI shows the model just two IDE
tools and this would be one of them, a tool that confidently reports clean is worse
than no tool. Shelling out to a linter is not a fix either: the CLI polls with
500ms/2000ms timeouts, and `tsc --noEmit` is an order of magnitude over that.

**A `String` buffer mirror, not a rope.** Edits cost one `memmove` plus an O(n)
line-index rebuild — microseconds at realistic file sizes, against human typing
speed. A rope would add a dependency and still have to materialise a contiguous
slice for the selection text. The real hazard is not edit cost but size, since
`"Plain Text"` means this attaches to any file someone drags in; documents over
4 MiB are simply not mirrored and fall back to disk.

## 9. Security

The socket is bound to `127.0.0.1` only, never `0.0.0.0`.

Every connection must present `X-Claude-Code-Ide-Authorization` matching a per-process
v4 UUID, compared in constant time. Failure closes with 1008 before the connection
reaches the MCP loop. This matters more than it might seem: the socket exposes your
selected source, and — through `at_mention_request` — the ability to push content
into your CLI prompt.

The token lives in the lock file, so that file is written `0600` inside a `0700`
directory, and written to a temporary name then renamed, so a reader can never
observe a half-written lock. The directory's permissions are tightened on startup
even if it already existed, since another tool may have created it with a laxer
umask.

## 10. Testing

The suite is built around two fakes. A **fake CLI** performs the real handshake —
subprotocol offer, auth header, `initialize`, `ide_connected`, `tools/list` — and
asserts what comes back. A **fake Zed** speaks LSP to the companion over an
in-memory pipe: `initialize`, `didOpen`, `didChange`, `codeAction`. Put together
in `tests/editor.rs`, they cover the whole path the feature rides on — a
code-action request on one side becomes `selection_changed` on the other — which
was previously checked only by watching a live session.

A third layer runs the built binary: `tests/cli.rs` pins the command-line surface,
and `tests/lifecycle_process.rs` sends it SIGINT, SIGTERM and stdin EOF, and
orphans it, checking each time that the lock file is gone and the process has
exited within a bound. `tests/lifecycle.rs` does the same for the in-process
`Handle`.

Covered: the 1008 rejection for a bad or missing token; that notifications draw no
response (including an explicit `"id": null`, which deserializes to `Some(Null)`
rather than `None`); the exact lock-file key set, permissions and pid liveness;
the exact key sets of `selection_changed` and `at_mentioned`; selection replay on
connect; the at-mention round trip; that discovery prefers our own live lock over
another editor's; UTF-16 position handling including surrogate pairs; incremental
document sync; the debounce under paused time; and lock removal on every exit.

Two habits worth keeping when adding to it:

*Revert the fix and confirm the test fails.* Six of the seven original handshake
assertions fail against the pre-fix behaviour. That check has already caught one
test here that passed against the very defect it was written for.

*Assert absence precisely.* `serde_json` returns `Value::Null` for a missing key, so
`is_null()` cannot distinguish "omitted" from "explicitly null" — a distinction this
protocol cares about. Use `contains_key`.

## 11. Limits

Structural, not a backlog:

- **No diff review.** `openDiff` needs an editor surface Zed exposes to extensions
  in no form. Every project in this lineage omits it.
- **No diagnostics.** §8.
- **Language-gated activation.** Inherent to `language_server_command`; mitigated
  by listing `"Plain Text"`, not solved.
- **Selection depends on Zed requesting code actions.** §5. Nothing in the LSP spec
  obliges Zed to keep doing this.
