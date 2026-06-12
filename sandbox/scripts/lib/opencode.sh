# opencode.sh — the authentication gate and the opencode server.

# `opencode serve` reads credentials at startup, so the operator gets a
# chance to log in before it starts — logging in later would mean restarting
# the entrypoint. Attached sessions get the interactive `opencode auth
# login`; detached ones block until credentials appear (log in from the host
# with `sbx exec -it <sandbox-name> opencode auth login`).
ensure_opencode_auth() {
    local auth_file="${XDG_DATA_HOME:-$HOME/.local/share}/opencode/auth.json"
    if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
        log "ANTHROPIC_API_KEY set — skipping opencode auth login"
    elif [ -s "$auth_file" ]; then
        log "opencode credentials found — skipping opencode auth login"
    elif [ -t 0 ]; then
        log "no opencode credentials — running 'opencode auth login'"
        opencode auth login \
            || log "WARNING: opencode auth login did not complete; agent runs will fail until you authenticate and restart"
    else
        log "no opencode credentials and no TTY — waiting before starting opencode serve"
        log "log in from the host:  sbx exec -it <sandbox-name> opencode auth login"
        until [ -s "$auth_file" ]; do
            sleep 5
        done
        log "opencode credentials detected — continuing"
    fi
}

start_opencode() {
    if wait_http "$OPENCODE_URL" 1; then
        log "opencode already running"
        return
    fi
    ensure_opencode_auth
    log "starting opencode serve"
    supervise opencode "$LOG_DIR/opencode.log" \
        opencode serve --hostname 127.0.0.1 --port 4096 &
    wait_http "$OPENCODE_URL" 60 \
        || die "opencode serve did not come up; see $LOG_DIR/opencode.log"
}
