# lily

A collaborative agent orchestrator inside Discord, written in Rust. lily is a
clone of [kimaki](https://github.com/remorses/kimaki)'s core: it connects
Discord to a local [OpenCode](https://opencode.ai) server so you can drive
coding agents from chat.

- **Channels are projects.** Each channel is linked to a project directory on
  the machine running lily.
- **Threads are sessions.** Every message you send in a project channel starts
  a thread bound to one OpenCode session. Messages in the thread continue it.

```
  Discord server              Your machine
 ┌──────────────────┐        ┌──────────────────────────────────────┐
 │ #web-app ────────┼────────┼─▶ /code/web-app ─▶ session (thread)  │
 │ #api ────────────┼────────┼─▶ /code/api     ─▶ session (thread)  │
 └──────────────────┘        │        ▲ reads, edits, runs commands │
       │ thread = session    │        ▼                             │
       ▼ agent replies ◀─────┼── OpenCode server (any model)        │
 └──────────────────┘        └──────────────────────────────────────┘
```

## Setup

1. Create a Discord bot at [discord.com/developers](https://discord.com/developers/applications),
   enable the **Message Content** intent, and invite it to your server with the
   `bot` and `applications.commands` scopes (send messages, create threads,
   manage threads).
2. Start an OpenCode server on the machine with your code: `opencode serve`
   (defaults to `127.0.0.1:4096`).
3. Run the bot:

```bash
export DISCORD_TOKEN=...      # bot token
cargo run --release -- run
```

4. In a Discord channel, run `/add-project directory:/code/web-app`
   (or from the CLI: `lily project add /code/web-app --channel <channel-id>`).
5. Send a message in the channel. lily creates a thread, starts a session in
   the project directory, and the agent replies in the thread (`⬥` for prose,
   `┣` for tool activity, a `-# lily ⋅ 2m 30s` footer per turn).

Configuration is environment-based: `DISCORD_TOKEN`, `OPENCODE_URL`
(default `http://127.0.0.1:4096`), `LILY_DATA_DIR` (default `~/.lily`),
`LILY_INTERRUPT_STEP_TIMEOUT_MS` (default `3000`), and `LILY_ALLOWED_USERS`
(comma-separated Discord user ids; when set, everyone else is ignored).

**Authorization:** the bot runs agents on the host machine, so lock it down.
Sensitive commands (`/add-project`, `/new-worktree`, `/merge-worktree`,
`/cancel-task`) default to members with **Manage Guild**; adjust per command in
Server Settings → Integrations. For private setups, set `LILY_ALLOWED_USERS`
to your own user id so no one else can start sessions at all.

## Message handling

A normal message sent while the agent is mid-run acts as an **interrupt**: if
the current step is still going after ~3 seconds, lily aborts it and delivers
your message, so a new instruction takes over instead of waiting behind a
long-running command.

## The queue

Line up a message to send **when the current run finishes** instead of
interrupting it:

- `/queue <message>`, or end any message with a punctuation mark plus `queue`
  (`fix the test. queue`, `commit it! queue`, or `queue` on its own final line).
  The suffix is stripped before the prompt reaches the agent.
- If the session is busy you get the queue position; if it is idle the message
  dispatches immediately.
- **Edit the queued Discord message** to update the queued prompt in place;
  remove the `queue` suffix and the item is dropped from the queue.
- When a queued message finally dispatches after waiting, it is shown as
  `» user: <prompt>`.
- `/clear-queue [position]` clears everything or one entry.

## btw (side questions)

Ask a clarifying question in parallel without disturbing the running task:
`/btw <prompt>`, or end a message with punctuation plus `btw`
(`why this approach? btw`). lily forks the session's **full context** into a
new `btw: <prompt>` thread and dispatches the question there immediately. The
original thread is never paused. Unlike `queue`, the `btw` suffix requires
punctuation or a newline before it.

## Worktrees

Move a session into an isolated git worktree so it never touches your main
checkout:

- `/new-worktree [name] [base-branch]` — from a thread, the name is derived
  from the thread title (long names are compressed by stripping vowels:
  `configurable-sidebar-width` → `cnfgrbl-sdbr-wdth`); from a channel, pass a
  name and lily creates the thread immediately. The branch is `lily/<name>`,
  the worktree lives under `~/.lily/worktrees/`, the thread gets a
  `⬦ worktree:` prefix, and the existing session context is forked into the
  worktree.
- `/merge-worktree [target-branch]` — rebases the worktree commits onto the
  target (default branch by default) and fast-forwards it, preserving all
  commits. Requires clean worktree and target. On conflicts, lily asks the
  agent **in the thread** to resolve them (`git add`, `git rebase --continue`),
  then you run `/merge-worktree` again. On success the worktree and branch are
  removed and the thread prefix is cleared.
- `/worktrees` — list the project's worktrees (lily-created or not).

## Scheduled tasks

Run a prompt once at a future UTC time or on a cron schedule. The task posts a
thread you can reply to, optionally starting an agent session:

```bash
# One-time (UTC ISO, must end in Z)
lily send --channel <channel-id> --prompt 'Review open PRs' \
  --send-at '2026-07-01T09:00:00Z'

# Recurring (cron, UTC): every Monday 9am
lily send --channel <channel-id> --prompt 'Run the test suite and summarize failures' \
  --send-at '0 9 * * 1'

# Reminder that does not start an AI session
lily send --channel <channel-id> --prompt 'Rotate the staging API key' \
  --send-at '2026-06-30T09:00:00Z' --notify-only

# Continue an existing thread on a schedule
lily send --thread <thread-id> --prompt 'Check the deploy status' --send-at '@hourly'
```

Manage tasks with `lily task list [--all]`, `lily task edit <id> [--prompt ...]
[--send-at ...]`, `lily task delete <id>`, or from Discord with `/tasks` and
`/cancel-task`. Without `--send-at`, `lily send` schedules the prompt to run on
the bot's next scheduler tick (~5s). The scheduler recovers tasks stranded in
`running` after a crash and reschedules cron tasks after each run.

## Slash commands

| Command | Description |
|---|---|
| `/add-project <directory>` | Link the current channel to a project directory |
| `/queue <message>` | Queue a message for after the current run |
| `/clear-queue [position]` | Clear the queue, or one entry |
| `/btw <prompt>` | Fork context into a new thread for a side question |
| `/new-worktree [name] [base-branch]` | Move the session into an isolated worktree |
| `/merge-worktree [target-branch]` | Rebase worktree commits back and clean up |
| `/worktrees` | List worktrees for the channel's project |
| `/tasks` | List scheduled tasks |
| `/cancel-task <id>` | Cancel a scheduled task |

## Architecture

The crate is layered domain-driven-design style. Dependencies point inward:
`domain` depends on nothing, `application` orchestrates domain rules over
connectors, `cli` wires it all together. The application layer talks to the
chat platform only through the `ChatConnector` port (thread/message ids are
opaque strings), so supporting another platform — e.g. Matrix via
matrix-rust-sdk, where rooms map to projects and Matrix threads to sessions —
means implementing one trait in a new connector.

```text
src/
  main.rs                        thin entry point
  cli/
    mod.rs                       clap commands: run / project / send / task
  domain/                        pure business rules, no I/O
    delivery.rs                  `. queue` / `. btw` suffix semantics
    rendering.rs                 2000-char markdown chunking, part rendering
    session.rs                   queued messages, assistant-turn parts
    task.rs                      schedule parsing (ISO/cron), task entities
    worktree.rs                  naming rules, layout, merge outcomes
  application/                   use cases
    chat.rs                      the chat port: what lily needs from a platform
    config.rs                    environment configuration
    session_runtime.rs           per-thread runtime: queue, interrupt, dispatch
    task_runner.rs               scheduled-task polling loop (5s tick, atomic claim)
  connector/                     adapters to the outside world
    discord.rs                   ChatConnector impl (serenity) + gateway events, slash commands
    opencode.rs                  OpenCode HTTP client + global SSE event listener
    sqlite.rs                    persistence: projects, sessions, worktrees, tasks
    git.rs                       worktree create / rebase-merge / list
```

State lives in SQLite at `~/.lily/lily.db`. The message queue is in-memory
per thread; everything needed to resume across restarts (channel links, thread
session ids, worktrees, tasks) is persisted.

Out of scope by design: remote-access features (browser control, tunnels,
screen sharing) — lily is a chat bot, not a remote desktop.

## Development

```bash
cargo test     # unit tests + git worktree integration tests
cargo clippy
```
