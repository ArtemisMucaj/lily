# util.sh — logging and small helpers shared by every boot step.

log() { printf '[lily] %s\n' "$*"; }
die() {
    log "ERROR: $*" >&2
    exit 1
}
gen_secret() { tr -dc 'A-Za-z0-9' </dev/urandom | head -c 48; true; }
# Succeeds once the URL answers any HTTP status (only connection refusal fails).
wait_http() {
    local url="$1" tries="${2:-60}" i=0
    until curl -s -o /dev/null --max-time 2 "$url"; do
        i=$((i + 1))
        [ "$i" -ge "$tries" ] && return 1
        sleep 1
    done
}
