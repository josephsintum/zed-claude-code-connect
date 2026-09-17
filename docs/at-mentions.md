# The @-mention hotkey

In VS Code, `cmd-alt-k` drops `@path#L12-20` into the Claude Code prompt for
whatever you have selected. This sets up the same thing in Zed.

It takes two small edits to your own Zed config, because a Zed extension cannot
register commands or keybindings at `schema_version = 1` — there is no such API
surface. A Zed **task** can run a command and be bound to a key, so that is the
route.

## How it works

The companion already tracks your live selection. The hotkey therefore does not
need to pass a range — it just says "mention what is selected", and the companion
turns its tracked selection into an `at_mentioned` notification.

That matters for correctness: Zed exposes `ZED_ROW` but nothing for the *other* end
of a selection, so a task passing coordinates could not tell whether the row it was
given is the anchor or the head. Reading the tracked selection avoids the question.

```
keypress ──> Zed task ──> claude-code-connect at-mention
                              │  discovers the companion via ~/.claude/ide/*.lock
                              ▼
                          companion ──at_mentioned──> claude
```

## Setup

### 1. Put the helper on your PATH

It is the same binary as the server, with a subcommand:

```sh
cargo build --release -p claude-code-connect
cp target/release/claude-code-connect ~/.local/bin/
```

### 2. Add the task

In `~/.config/zed/tasks.json`, append to the array:

```json
{
  "label": "Claude: mention selection",
  "command": "claude-code-connect",
  "args": ["at-mention", "--worktree", "$ZED_WORKTREE_ROOT"],
  "use_new_terminal": false,
  "allow_concurrent_runs": true,
  "reveal": "never",
  "hide": "always",
  "save": "none",
  "show_summary": false,
  "show_command": false
}
```

`reveal: never` and `hide: always` keep it from stealing focus or leaving a terminal
tab behind. `save: none` matters — the companion reads your *unsaved* buffer, so
there is nothing to flush first.

### 3. Bind a key

In `~/.config/zed/keymap.json`:

```json
{
  "context": "Editor && mode == full",
  "bindings": {
    "cmd-alt-k": ["task::Spawn", { "task_name": "Claude: mention selection" }]
  }
}
```

`cmd-alt-k` mirrors VS Code. If your `base_keymap` is JetBrains or Sublime, check
for a conflict first with `zed: open default keymap` — `cmd-shift-a` and
`cmd-escape` are other reasonable choices.

## Checking it works

```sh
# From inside the project, with it open in Zed:
claude-code-connect at-mention --worktree "$PWD"
```

Select something in Zed first. The CLI's prompt should gain `@path#L12-20`. Zed
itself shows nothing: the task runs hidden and the companion has no way to post a
message back into the editor. To watch the wire directly:

```sh
cargo run -p claude-code-connect --example watch -- /path/to/project
```

## If nothing happens

- **"no running companion covers …"** — the project is not open in Zed, or no file
  has been opened in it yet, so the language server has not started.
- **Nothing in the CLI** — check `claude` is connected: run `/ide`, which should
  show **Zed**. The at-mention goes to whatever clients are attached.
- **Mentions the wrong lines** — the companion mentions its *tracked* selection. If
  the editor lost focus after you selected, the tracked selection is still the last
  one Zed reported, which is normally what you want.
