# Troubleshooting the lily Docker Sandbox

Setup and architecture are documented in [README.md](README.md). Service logs
are shared with the host at `~/.lily/sandbox/logs/` (`tuwunel.log`,
`opencode.log`, `ngrok.log`) — start there.

## A service keeps restarting

The entrypoint supervises every service (Tuwunel, opencode, ngrok, and lily
itself) and restarts crashes with exponential backoff — 2s doubling up to
60s, reset after a healthy minute. Repeated `restarting in ...` lines in the
agent session mean a service is crash-looping: check its log under
`~/.lily/sandbox/logs/` for the underlying error (a bad `config.env` value,
a missing provider API key for opencode, or an invalid `NGROK_AUTHTOKEN` are
the usual suspects).

## Tuwunel won't start / database errors on the shared mount

RocksDB can be picky about locking and mmap on workspace passthrough
filesystems. Set `LILY_SANDBOX_MATRIX_DATA=local` in `config.env` and remove
`~/.lily/sandbox/matrix` + `credentials.env`; the homeserver then lives on
the sandbox disk (still persistent, just not host-visible).

## ngrok can't connect

Sandbox egress runs through a policy proxy; allow the ngrok domains (the kit
declares them) or relax the sandbox's network policy in the `sbx` dashboard.

## Changing `server_name`

Matrix bakes the server name into every user id; it cannot change after
first boot. To start over, delete `~/.lily/sandbox/matrix/` and
`~/.lily/sandbox/credentials.env` (and `~/.lily/matrix-store/` +
`matrix-session.json` so the bot re-logs-in).

## Full reset

`sbx rm <name>` and delete the paths above; project folders and the rest of
`~/.lily` are untouched.
