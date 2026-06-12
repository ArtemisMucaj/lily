# lily in a Docker Sandbox

Run the **entire lily experience** — a private Matrix homeserver, an
[OpenCode](https://opencode.ai) server, an optional ngrok tunnel, and lily
itself — inside one [Docker Sandbox](https://docs.docker.com/ai/sandboxes/)
microVM. The agent edits only the project folders you mount; everything else
on your machine is out of reach.

```
┌─ Docker Sandbox (microVM) ──────────────────────────────┐
│  Tuwunel (Matrix)  ←─ lily ─→  opencode serve           │
│   :8008  ▲  ▲                      :4096                 │
└──────────│──│────────────────────────────────────────────┘
           │  └── ngrok tunnel ──→ your phone (Element)
           └── sbx ports 8008  ──→ localhost clients
   shared workspaces: ~/.lily  +  your project folders
```

## Security model

This setup is **single-tenant by construction** — the goal is that exactly one
person (you) can reach the homeserver and drive the agent:

The homeserver is [Tuwunel](https://github.com/matrix-construct/tuwunel)
(conduwuit's official successor): a single lightweight Rust binary, no
Python, idles in the tens of megabytes.

- **No federation.** `allow_federation = false` — the server never speaks the
  federation protocol, inbound or outbound. The `server_name` (default
  `lily.localhost`) is private and never needs to resolve publicly.
- **Registration closed.** Registration requires a token generated inside the
  sandbox and stored with `0600` permissions; without it the register
  endpoint refuses. First boot uses that token to create exactly two
  accounts: you (`@owner:lily.localhost`, granted server admin as the first
  registration) and the bot (`@lily:lily.localhost`).
- **Allowlist pinned to you.** `LILY_ALLOWED_USERS` is set to your MXID
  automatically, so lily ignores every other account — defense in depth even
  if another account ever existed.
- **Remote access goes through ngrok with Matrix auth in front.** Anyone who
  discovers the tunnel URL hits a login wall with no registration token and
  no public room directory. Local access via `sbx ports` publishes only on
  your host.
- The whole homeserver config (`tuwunel.toml`) is rewritten on **every boot**,
  so the lockdown cannot drift.

## Prerequisites

- Docker Desktop with [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/)
  and the `sbx` CLI
- An API key for the model provider opencode should use (e.g.
  `ANTHROPIC_API_KEY`)
- Optional: an [ngrok](https://ngrok.com) account for remote access — claim
  your free **static domain** so the homeserver URL survives restarts

## Setup

**1. Build the template image** (from the repository root):

```bash
docker build -f sandbox/Dockerfile -t lily-sandbox:latest .
docker image save lily-sandbox:latest -o lily-sandbox.tar
sbx template load lily-sandbox.tar
```

**2. Configure** — create `~/.lily/sandbox/config.env` on the host
(`chmod 600` it; `~/.lily` is shared with the sandbox, so the stack picks it
up at boot):

```bash
ANTHROPIC_API_KEY=sk-ant-...

# Remote access (optional — omit both for local-only):
NGROK_AUTHTOKEN=...
NGROK_DOMAIN=your-name.ngrok-free.app   # your static domain; strongly recommended

# Optional overrides (defaults shown):
# LILY_MATRIX_SERVER_NAME=lily.localhost  # immutable after first boot!
# LILY_OWNER_USER=owner                   # your account's localpart
# LILY_MATRIX_BOT_USER=lily               # the bot account's localpart
# LILY_SANDBOX_MATRIX_DATA=shared         # 'local' keeps Matrix DBs on sandbox disk
# DISCORD_TOKEN=...                       # also enable the Discord connector
```

**3. Run** — mount your project folder(s) plus `~/.lily`:

```bash
sbx run --kit ./sandbox/kit lily ~/code/my-project ~/.lily
```

Folders appear at their host absolute paths inside the sandbox, so lily's
channel↔directory mappings and worktree hashes work identically on both
sides. Add more projects or `:ro` mounts as extra arguments.

On first boot the stack generates the homeserver, creates your account and
the bot account, and prints a banner with your login and (if configured) the
ngrok URL. Your password lands in `~/.lily/sandbox/credentials.env` on the
host (`0600`).

**4. Connect a Matrix client** (e.g. Element):

- *Locally:* `sbx ports <sandbox-name> --publish 8008:8008`, then use
  `http://localhost:8008` as the homeserver URL.
- *Remotely:* use `https://<your-ngrok-domain>` as the homeserver URL.

Log in as `@owner:lily.localhost` with the generated password, create a room,
and invite `@lily:lily.localhost` — lily auto-joins. Then link the room to a
mounted project:

```
!add-project /home/you/code/my-project
```

Send a message in the room and the agent goes to work — inside the sandbox.

## Day-to-day

```bash
sbx ls                                   # status
sbx stop lily-... / sbx run ...          # pause / resume (state persists)
sbx exec -it <name> bash                 # shell into the stack
```

Interactive shells inherit the stack's environment (data dir, opencode URL),
so the lily CLI works directly:

```bash
sbx exec -it <name> bash
lily project list
lily send --channel <room-id> --prompt "daily report" --send-at "0 9 * * 1-5"
```

If a service crashes, the entrypoint restarts it automatically with
exponential backoff (2s doubling up to 60s, reset after a healthy minute) —
this covers Tuwunel, opencode, ngrok, and lily itself.

Service logs are shared with the host at `~/.lily/sandbox/logs/`
(`tuwunel.log`, `opencode.log`, `ngrok.log`).

## What lives where

| Path (host = sandbox) | Contents |
|---|---|
| `~/.lily/lily.db`, `~/.lily/worktrees/` | lily's normal state |
| `~/.lily/matrix-store/`, `matrix-session.json` | lily's Matrix client session |
| `~/.lily/sandbox/config.env` | your settings (host-editable) |
| `~/.lily/sandbox/credentials.env` | generated account passwords (`0600`) |
| `~/.lily/sandbox/matrix/` | the homeserver: `tuwunel.toml`, registration token, RocksDB database |
| `~/.lily/sandbox/logs/` | service logs |

## Troubleshooting

See [TROUBLESHOOTING.md](TROUBLESHOOTING.md): crash-looping services,
RocksDB on the shared mount, ngrok egress, changing `server_name`, and how
to do a full reset.

