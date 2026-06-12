# Troubleshooting the lily Docker Sandbox

Service logs are shared with the host at `~/.lily/sandbox/logs/` — start
there. Repeated `restarting in ...` lines in the agent session mean a
service is crash-looping; its log names the underlying error (a bad
`config.env` value, missing opencode credentials, an invalid
`NGROK_AUTHTOKEN`).

## When in doubt: reset and start from scratch

Everything the stack needs is recreated on the next boot, so the cure for a
wedged setup is a clean slate. Two things are worth keeping: your
hand-written settings, and `~/.lily/worktrees/` — **check it for unmerged
branches before deleting**. Everything else is cheap:

- `lily.db` — room↔project links, thread↔session bindings, scheduled tasks;
  recreate links with `!add-project`, threads just start fresh sessions
- `matrix-session.json`, `matrix-store/` — the bot's Matrix login; it logs
  back in by itself
- `sandbox/credentials.env` + the homeserver database — accounts, rooms and
  history; you get a new password (the boot banner says where) and re-create
  the room

```bash
sbx rm <sandbox-name>                      # sandbox/lilyctl status shows it
mv ~/.lily/sandbox/config.env /tmp/        # keep your settings
rm -rf ~/.lily
mkdir -p ~/.lily/sandbox && mv /tmp/config.env ~/.lily/sandbox/
sandbox/lilyctl up ~/code/my-project
```

## ngrok can't connect

Sandbox egress runs through a policy proxy; allow the ngrok domains (the kit
declares them) or relax the sandbox's network policy in the `sbx` dashboard.
