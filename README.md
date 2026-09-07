[Website](https://aronym5.github.io/aidev/)

# aidev

A **blazingly fast**, **static** AI coding agent for your terminal — written in **Rust**, with
**no library dependencies** and a small footprint (compiled &lt; 7 MB), so it runs on a wide
range of distributions and versions. It chats with an [OpenAI-compatible streaming
endpoint](#configuration) and acts inside a **per-session sandbox**: searching, reading, editing,
and running code through explicit permissions.

## Highlights

- **Multi-session command center** — run several conversations side by side, each with its own
  prompt, history, and scroll state.
- **Podman sandboxing** — every session executes inside an isolated container channel; the model
  never touches your host machine directly.
- **Git & worktrees** — develop several branches of the same project concurrently across multiple
  sessions, containers, worktrees, and branches, without collisions.
- **Full access control** — choose read, write, or execute per turn; every tool call is gated by
  the permission you grant.
- **Zoom levels** — step between detail, dialog, and full overview for the right view at the
  right resolution.
- **Full context insight** — see exactly what the model sees and compact the context window when
  you need to.
- **Zero dependencies** — one static, portable binary around 7 MB; drop it anywhere and it runs.

---

Install with

```sh
curl -sSL https://aronym5.github.io/aidev/install.sh | sh
```

---

## Overview

aidev is a full-screen chat harness for driving a coding model against a real project.
Each conversation ("session") lives in its own tab and can be bound to a **channel** — a
sandboxed working directory with a shell. From that point the model can invoke tools
(`grep`, `glob`, `read`, `webfetch`, `edit`, `write`, `run`), every call gated by the
permission you grant for that turn.

- Markdown rendering — bold, inline code, lists, and fenced code with syntax highlighting.
- Tool calls and their results are logged inline; `run`/`edit` produce console/diff boxes with
  live progress.
- Streaming answers, context-length handling, and automatic retries with backoff.
- Full-screen TUI: status line, tab bar, channel indicator, and a zoomable overview ↔ dialog
  view.

### Safety & flexibility

- **Every session runs in its own sandbox.** A channel backs each session with an
  isolated working copy — either a container (podman) or a host directory (`local`) —
  so the model can act without touching your machine directly.
- **Flexible bridge between dialog and code.** The chat is not a passive transcript:
  bound sessions let the model read and write the actual codebase, run builds/tests, and
  show the results inline. You stay in control through permissions and a picker per
  turn.
- **podman + git integrated.** Container channels isolate execution and filesystem
  effects; git worktrees give you a cheap, isolated copy per session/branch. The two
  compose: duplicate a channel to get its own worktree-backed container.

---

## Sessions & Tabs

Run several conversations side by side. The tab bar (visible from the second session on)
lets you switch instantly; each session keeps its own prompt, history, and scroll state.
The active tab is highlighted; a tab with an in-flight agent shows a waiting animation.

---

## Channels

A **channel** is the interface to a filesystem and shell. Sessions without a channel are
pure chat; once bound, the model can act in that working directory.

Two channel types:

- **podman** — runs in a container started from an image. *Run* mode creates its own
  container (duplicateable per session); *attach* mode joins an existing container.
  Commands run as your host UID with keep-id, so file permissions match the host.
- **local** — runs directly on the host (no container). Practical in sandboxes/CI where
  rootless podman is unavailable.

The status line shows a channel indicator: green = running, yellow = starting, red =
problem, grey = not yet checked.

All paths are relative to the channel root; escapes (`..`) and out-of-root symlinks are
rejected. The `config.toml` can pre-register channel **paths** (optionally with a
container image) that the channel builder offers — see [Configuration](#configuration).

---

## Permissions

Before sending a turn you choose a permission level (`Tab` cycles read → write →
execute). The levels build on each other:

| Level     | Model may use                          |
|-----------|----------------------------------------|
| `read`    | `grep`, `glob`, `read`, `webfetch`     |
| `write`   | + `write`, `edit`                      |
| `execute` | + `run`                                |

The prompt, input, and sent message are tinted by the permission color. Local channels
default to `read`, podman channels to `execute`; your last choice is remembered.
Executing on a `local` channel asks for an explicit one-time confirmation (take
responsibility, or confirm each `exec` call).

---

## Repositories & Worktrees

Two ways to get an isolated working copy backed by git:

- **Duplicate a channel** (`Alt+D`) — clones the channel into its own git worktree and
  container, so experiments don't collide.
- **`/branch <name>`** — explicitly create a new worktree, channel, and session for a
  branch.
- **`/commit <msg>`** — commit the current worktree's changes.

These isolate changes and make review easy while sharing the same repository underneath.

---

## Key bindings (essentials)

New to a project? Open the channel picker with `Alt+C` — either choose an existing channel
or build a new one via the "new channel" entry.

| Key                      | Action                                                         |
|--------------------------|----------------------------------------------------------------|
| `Enter`                  | Send message / confirm in dialog                               |
| `Esc`                    | Cancel running response / abort / close dialog                 |
| `Ctrl+N`                 | New session / tab                                              |
| `Ctrl+D` / `Ctrl+W`      | Close session / tab                                            |
| `Alt+←` / `Alt+→`        | Switch session / tab (or `Ctrl+1`…`Ctrl+9`)                    |
| `Alt+C`                  | Open channel picker (choose / manage / build new)              |
| `Alt+M`                  | Open model picker (same as `/model` without argument)          |
| `Alt+D`                  | Duplicate channel (own worktree)                               |
| `Ctrl+O`                 | Options dialog                                                 |
| `Alt++` / `Alt+-`        | Zoom in / Zoom out (detail / dialog / overview)                |
| `Tab`                    | Change permission (read/write/execute, needs a channel)        |
| `PgUp` / `PgDn`          | Scroll chat history                                            |

The input field supports multi-line editing, word-wrap, and cursor/selection movement
with the arrow keys.

### Slash commands

| Command            | Effect                                                        |
|--------------------|---------------------------------------------------------------|
| `/run <expr>`      | Run a shell expression directly through the channel, no LLM   |
| `/compact`         | Manually compact the context history                          |
| `/model [alias]`   | Switch the model (no argument opens the picker)               |
| `/channel`         | Open channel picker (same as `Alt+C`)                         |
| `/options`         | Open options dialog (same as `Ctrl+O`)                        |
| `/branch <name>`   | Create a new worktree + channel + session                     |
| `/commit <msg>`    | Commit the current worktree's changes                         |

---

## Project layout

```
src/
├── main.rs     entry point: raw mode, alt-screen
├── app/        app state, event loop, sessions, dialogs
├── ui/         rendering (markdown chat, input, status bar, pickers)
├── editor.rs   reusable multi-line input field
├── channel/    channel trait: local, podman (attach/run), search, glob, shell
├── llm/        SSE streaming, retry/backoff, tool definitions & execution, worker loop
├── repo.rs     git-worktree management for session duplicates
├── diff.rs     exact substring edit + two-column diff view
├── perm.rs     permission levels read → write → execute
├── text.rs     shared text truncation helpers
└── config.rs   config lookup & loading (TOML)
```

---

## Configuration

Config is read in order (first hit wins):

1. `$XDG_CONFIG_HOME/aidev/config.toml`
2. `~/.config/aidev/config.toml`

```toml
# Default model – always "provider/name"
model = "zen/big-pickle"

# Providers: key = prefix used in the model IDs
[provider.zen]
base_url = "https://opencode.ai/zen/v1"   # OpenAI-compatible endpoint
api_key  = "public"

[provider.ollama]
base_url = "http://localhost:11434/v1"    # no key needed

# Named models, choosable per session with /model or alt+m
[models]
smart = "zen/big-pickle"                  # short form: just the model ID

[models.local]                            # long form with context window
id = "ollama/llama3"
context_window = 8192

# (Host) paths for the channel builder, linked with a default podman image
[path."/path/to/projectA"]
image = "node:22"

[path."/path/to/projectB"]
image = "alpine"
```
---

## Requirements, build & run

The released binary has **no external library dependencies** — it is statically compiled
(&lt; 7 MB) and portable, so the install step above needs nothing but `curl` and `sh`.

To build from source:

```sh
cargo build --release     # binary at target/release/aidev
cargo run
cargo test                # unit tests live next to the sources
```

To run (all optional, but the app will refuse to work in some areas if not available):

- an OpenAI-compatible endpoint
- **podman** for container channels.
- **git** for worktree-based duplicates and `/branch` / `/commit`
- **rg** for fast search, falls back to `grep` if absent.

## License

MIT
