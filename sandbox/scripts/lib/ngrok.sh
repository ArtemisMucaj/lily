# ngrok.sh — optional remote tunnel to the Matrix port. Only opened when
# NGROK_AUTHTOKEN is set; leaves NGROK_URL for the banner.

NGROK_URL=""

start_ngrok() {
    if [ -z "${NGROK_AUTHTOKEN:-}" ]; then
        log "NGROK_AUTHTOKEN not set — skipping remote tunnel (local access only)"
        return
    fi
    if wait_http "http://127.0.0.1:4040/api/tunnels" 1; then
        log "ngrok already running"
    else
        log "starting ngrok tunnel"
        local ngrok_args=(http 8008 --log stdout)
        [ -n "${NGROK_DOMAIN:-}" ] && ngrok_args+=(--url "https://$NGROK_DOMAIN")
        supervise ngrok "$LOG_DIR/ngrok.log" ngrok "${ngrok_args[@]}" &
    fi
    for _ in $(seq 1 30); do
        NGROK_URL="$(curl -s --max-time 2 http://127.0.0.1:4040/api/tunnels 2>/dev/null \
            | jq -r '.tunnels[0].public_url // empty')"
        [ -n "$NGROK_URL" ] && break
        sleep 1
    done
    [ -n "$NGROK_URL" ] || log "WARNING: no ngrok tunnel URL yet; see $LOG_DIR/ngrok.log"
}
