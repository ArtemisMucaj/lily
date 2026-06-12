# config.sh — resolve the data dir, load host-side settings, and derive the
# globals every other boot step uses.

HS_URL="http://127.0.0.1:8008"

load_config() {
    # Docker Sandboxes mount workspaces at their host absolute paths, so when
    # the host's ~/.lily is passed as a workspace it shows up as a mount
    # whose path ends in /.lily.
    if [ -z "${LILY_DATA_DIR:-}" ]; then
        local mounted
        mounted="$(awk '$2 ~ /\/\.lily$/ { print $2; exit }' /proc/mounts || true)"
        LILY_DATA_DIR="${mounted:-$HOME/.lily}"
    fi
    export LILY_DATA_DIR
    SANDBOX_DIR="$LILY_DATA_DIR/sandbox"
    LOG_DIR="$SANDBOX_DIR/logs"
    mkdir -p "$SANDBOX_DIR" "$LOG_DIR"
    chmod 700 "$SANDBOX_DIR"
    log "data dir: $LILY_DATA_DIR"

    if [ -f "$SANDBOX_DIR/config.env" ]; then
        log "loading $SANDBOX_DIR/config.env"
        set -a
        # shellcheck disable=SC1091
        . "$SANDBOX_DIR/config.env"
        set +a
    fi

    SERVER_NAME="${LILY_MATRIX_SERVER_NAME:-lily.localhost}"
    BOT_LOCALPART="${LILY_MATRIX_BOT_USER:-lily}"
    OWNER_LOCALPART="${LILY_OWNER_USER:-owner}"
    BOT_MXID="@$BOT_LOCALPART:$SERVER_NAME"
    OWNER_MXID="@$OWNER_LOCALPART:$SERVER_NAME"

    # Matrix data is shared with the host by default. `local` keeps the live
    # database on the sandbox disk instead — the escape hatch if RocksDB
    # misbehaves on the passthrough filesystem.
    case "${LILY_SANDBOX_MATRIX_DATA:-shared}" in
        shared) MATRIX_DIR="$SANDBOX_DIR/matrix" ;;
        local) MATRIX_DIR="$HOME/.lily-matrix" ;;
        *) die "LILY_SANDBOX_MATRIX_DATA must be 'shared' or 'local'" ;;
    esac

    export OPENCODE_URL="${OPENCODE_URL:-http://127.0.0.1:4096}"
}
