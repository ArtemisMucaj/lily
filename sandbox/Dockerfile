# Template image for running the full lily stack inside a Docker Sandbox
# (https://docs.docker.com/ai/sandboxes/). Build from the repository root:
#
#   docker build -f sandbox/Dockerfile -t lily-sandbox:latest .
#
# The image extends the official `opencode` sandbox template (which ships the
# opencode agent and satisfies the sandbox base-image contract: `agent` user at
# UID 1000 with passwordless sudo) and adds lily, the Tuwunel Matrix
# homeserver, and the ngrok agent. The kit in sandbox/kit/ boots everything
# via the entrypoint script.

FROM rust:1 AS lily-builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM docker/sandbox-templates:opencode

USER root

# Tuwunel (https://github.com/matrix-construct/tuwunel) — lightweight Matrix
# homeserver, single binary, conduwuit's official successor.
ARG TUWUNEL_VERSION=1.7.1
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl jq ca-certificates \
    && case "$(dpkg --print-architecture)" in \
        amd64) TUWUNEL_ARCH=x86_64-v1 ;; \
        arm64) TUWUNEL_ARCH=aarch64-v8 ;; \
        *) echo "unsupported architecture for tuwunel" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o /tmp/tuwunel.deb \
        "https://github.com/matrix-construct/tuwunel/releases/download/v${TUWUNEL_VERSION}/v${TUWUNEL_VERSION}-release-all-${TUWUNEL_ARCH}-linux-gnu-tuwunel.deb" \
    && apt-get install -y --no-install-recommends /tmp/tuwunel.deb \
    && rm -f /tmp/tuwunel.deb \
    && rm -rf /var/lib/apt/lists/*

# ngrok agent (v3); the tunnel is only opened when NGROK_AUTHTOKEN is set.
RUN ARCH="$(dpkg --print-architecture)" \
    && curl -fsSL "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-linux-${ARCH}.tgz" \
       | tar -xz -C /usr/local/bin ngrok

COPY --from=lily-builder /src/target/release/lily /usr/local/bin/lily
COPY sandbox/scripts/entrypoint /usr/local/bin/entrypoint
RUN chmod 0755 /usr/local/bin/lily /usr/local/bin/entrypoint

USER agent
