# Template image for running the full lily stack inside a Docker Sandbox
# (https://docs.docker.com/ai/sandboxes/). Build from the repository root:
#
#   docker build -f sandbox/Dockerfile -t lily-sandbox:latest .
#
# The image extends the official `opencode` sandbox template (which ships the
# opencode agent and satisfies the sandbox base-image contract: `agent` user at
# UID 1000 with passwordless sudo) and adds lily, a Synapse Matrix homeserver,
# and the ngrok agent. The kit in sandbox/kit/ boots everything via
# `lily-sandbox-up`.

FROM rust:1 AS lily-builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM docker/sandbox-templates:opencode

USER root

# Synapse lives in its own venv so it never collides with project tooling.
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 python3-venv curl jq ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/synapse \
    && /opt/synapse/bin/pip install --no-cache-dir --upgrade pip \
    && /opt/synapse/bin/pip install --no-cache-dir matrix-synapse

# ngrok agent (v3); the tunnel is only opened when NGROK_AUTHTOKEN is set.
RUN ARCH="$(dpkg --print-architecture)" \
    && curl -fsSL "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-linux-${ARCH}.tgz" \
       | tar -xz -C /usr/local/bin ngrok

COPY --from=lily-builder /src/target/release/lily /usr/local/bin/lily
COPY sandbox/scripts/lily-sandbox-up /usr/local/bin/lily-sandbox-up
COPY sandbox/scripts/lilyctl /usr/local/bin/lilyctl
RUN chmod 0755 /usr/local/bin/lily /usr/local/bin/lily-sandbox-up /usr/local/bin/lilyctl

USER agent
