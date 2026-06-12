# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

**lily** is a collaborative agent orchestrator written in Rust that bridges Discord and/or Matrix to a local OpenCode server. Channels map to project directories; threads map to coding sessions. Sending a message in a project channel causes an AI agent to edit code on the local machine and reply in a thread.

## Commands

```bash
# Build
cargo build
cargo build --release

# Test (all tests, including git integration tests that use real temp repos)
cargo test
cargo test --verbose

# Run a single test by name
cargo test <test_name>
cargo test delivery::tests::queue_with_period

# Lint
cargo clippy

# Run the bot
DISCORD_TOKEN=... cargo run -- run

# Project management (no bot needed)
cargo run -- project add /path/to/project --channel <discord-channel-id>
cargo run -- project list

# Send a prompt (bot picks it up on its next poll)
cargo run -- send --channel <id> --prompt "fix the failing test"
cargo run -- send --thread <id> --prompt "what does this function do"
cargo run -- send --channel <id> --prompt "daily report" --send-at "0 9 * * 1-5"  # cron

# Manage scheduled tasks
cargo run -- task list
cargo run -- task list --all
cargo run -- task delete <id>
cargo run -- task edit <id> --prompt "new text" --send-at "2026-01-01T09:00:00Z"
```

## Architecture

The codebase is strict domain-driven design with inward-pointing dependencies:

```
cli → application → domain        (no I/O in domain)
         ↓
    connector  (adapts domain/application to external systems)
```

- **`domain/`** — pure business rules with no I/O: delivery parsing, rendering, worktree naming, task scheduling, session queue state
- **`application/`** — use cases: `session_runtime` (per-thread orchestration), `task_runner` (scheduled task loop), `commands` (slash command handlers), `chat` (platform-agnostic `ChatConnector` trait), `config` (env-var config)
- **`connector/`** — adapters: `discord` (serenity), `matrix` (matrix-sdk), `opencode` (HTTP+SSE), `sqlite` (persistence), `git` (worktree management), `router` (routes to Discord or Matrix by id shape)
- **`cli/`** — clap argument parsing and composition root; `run_bot` wires everything together

### Key data flows

**Incoming message → OpenCode run:**
`discord::Handler` / `matrix::run` → `session_runtime::get_or_create_runtime` → `ThreadRuntime::enqueue` → delivery parsing → OpenCode HTTP call → SSE event stream → `rendering::split_markdown` → platform message send

**Scheduled task → chat:**
`task_runner::run_task_loop` polls SQLite every 30s → fires `TaskPayload` → `router::RoutedChat` routes by id shape → `session_runtime::dispatch`

**Worktree creation:**
Thread title → `domain::worktree::slugify` (FNV-1a64 hash of project dir + slug) → `connector::git::create_worktree` → path `~/.lily/worktrees/<project-hash>/<slug>`

### Session/thread runtime

`AppState` holds a `HashMap<thread_id, Arc<ThreadRuntime>>`. Each `ThreadRuntime` has a `Mutex<RuntimeState>` with `session_id`, `busy` flag, and a `VecDeque<QueuedMessage>`. The runtime is never replaced for a live thread; if the working directory changes (worktree ready/merged), the runtime is retargeted in place so the queue carries over.

### OpenCode client

Single global SSE listener on `/` feeds all per-thread renderers. Sessions are created via POST, messages via POST, forks via POST. The event stream delivers tool-call progress and text chunks.

## Message delivery semantics

`domain::delivery` parses suffixes from user messages before they reach OpenCode:

| Suffix | Behavior |
|---|---|
| `<punc> queue` or `\nqueue` | Wait for current run to finish, then dispatch (prefixed with `» `) |
| `<punc> btw` or `\nbtw` | Fork full session context into a new parallel thread |
| _(none)_ | Interrupt current run after grace period (`LILY_INTERRUPT_STEP_TIMEOUT_MS`, default 3000ms) |

Note: `btw fix this` (no separator) is **not** treated as btw. `queue` as the entire message is **not** treated as queue.

## Rendering markers

Defined in `domain::rendering`:

- `⬥ ` — agent prose responses
- `┣ ` — tool/progress lines
- `» ` — queued messages dispatched after a wait

Discord messages are capped at 2000 chars; `split_markdown` splits at line boundaries and keeps code fences balanced across chunks.

## Configuration (environment variables)

| Variable | Default | Purpose |
|---|---|---|
| `DISCORD_TOKEN` | — | Discord bot token; enables Discord connector |
| `OPENCODE_URL` | `http://127.0.0.1:4096` | OpenCode server base URL |
| `LILY_DATA_DIR` | `~/.lily` | SQLite database and worktree root |
| `LILY_INTERRUPT_STEP_TIMEOUT_MS` | `3000` | Grace period before interrupting a run |
| `LILY_ALLOWED_USERS` | _(empty = everyone)_ | Comma-separated Discord snowflakes or Matrix MXIDs |
| `MATRIX_HOMESERVER` | — | Enables Matrix connector when set |
| `MATRIX_USER` | — | Matrix user id or localpart |
| `MATRIX_PASSWORD` | — | Matrix account password |

Log level is controlled via `RUST_LOG` (default: `info,serenity=warn`).

## SQLite schema

`connector::sqlite::Db` stores: channel↔directory mappings, thread↔session id bindings, worktree state (pending/ready/merged), and scheduled tasks. The DB uses WAL journaling. Schema is created via `CREATE TABLE IF NOT EXISTS` on `Db::open`.

## Testing patterns

- Domain tests are inline `#[test]` modules (pure, no I/O)
- Git integration tests use real temp repos created with `git init` in `tempdir`
- Async tests use `#[tokio::test]`
- Session runtime tests verify queue mechanics, interrupt handling, and edit flows
