# CLAUDE.md

Guidance for Claude Code when working in this repository.

Read [docs/architecture.md](docs/architecture.md) before changing anything in
`zed-claude-ide-server/`. The protocol this implements is undocumented and was
reconstructed by reading shipped binaries; several parts look arbitrary and are not.

## Things that will bite you

- **The editor is the server, the CLI is the client.** Not the other way round.
- **The port comes from the lock file's *name*.** A port field in the JSON is ignored.
- **`ide_connected` is a notification** that does not begin with `notifications/`.
  Guard on the absence of an `id`, and treat an explicit `"id": null` as absent —
  `Option<Value>` deserializes that to `Some(Value::Null)`, not `None`.
- **Optional wire fields must be omitted, not null.** The CLI validates with an
  optional number, which accepts a missing key and rejects null.
- **Live selection depends on Zed issuing `textDocument/codeAction`** as the cursor
  moves. That is the entire mechanism; there is no LSP selection notification.
- **This server attaches to every buffer.** Anything it advertises is a request Zed
  routes to it instead of, or alongside, the project's real language server. Do not
  advertise a capability without a handler, and do not add one that is not needed
  for selection tracking.

## Commits

Describe the behaviour that changed and why it was wrong before. The reasoning
behind a protocol fix is usually not recoverable from the diff, because the
specification is a binary someone has to re-read.

No ticket ids or commit hashes in code, comments, or test names — state the
invariant instead. Existing ticket-prefixed comments are institutional knowledge:
leave them.

## Tests

- **Revert the fix and confirm the test fails.** A test that passes against the
  defect it was written for is worth nothing. That has already happened here once.
- **Assert absence precisely.** `serde_json` returns `Value::Null` for a missing key,
  so `is_null()` cannot distinguish "omitted" from "explicitly null".
- Prefer adding to the fake-CLI harness in `tests/handshake.rs`, or the fake-Zed rig
  in `tests/editor.rs`, over unit-testing a handler in isolation; they exercise the
  path the real CLI and the real editor take.

## Before claiming something works

`cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`, and
`cargo fmt --all -- --check` all pass in CI. For anything touching the wire, also
watch it live:

```sh
cargo run -p zed-claude-ide-server --example watch -- /path/to/project
```
