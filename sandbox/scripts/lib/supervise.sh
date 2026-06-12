# supervise.sh — keep a service alive with exponential backoff (2s doubling
# up to 60s; the backoff resets once a run survives for a minute).

# Runs in the background; the TERM/INT trap forwards the signal to the
# current child so shutdown stays clean. The backoff sleep is backgrounded
# too — bash defers traps while a foreground command runs, and `wait` is the
# only spot where signals interrupt immediately.
supervise() { # name logfile cmd...
    local name="$1" logfile="$2" child=0 delay=2 started rc
    shift 2
    trap '[ "$child" != 0 ] && kill "$child" 2>/dev/null; exit 0' TERM INT
    while :; do
        started="$(date +%s)"
        "$@" >>"$logfile" 2>&1 &
        child=$!
        rc=0
        wait "$child" || rc=$?
        if [ $(($(date +%s) - started)) -ge 60 ]; then
            delay=2
        fi
        log "$name exited (status $rc); restarting in ${delay}s (log: $logfile)"
        sleep "$delay" &
        child=$!
        wait "$child" || true
        delay=$((delay * 2 > 60 ? 60 : delay * 2))
    done
}
