# matrix.sh — the locked-down Tuwunel homeserver and its two accounts.

find_tuwunel() {
    TUWUNEL="$(command -v tuwunel || true)"
    if [ -z "$TUWUNEL" ]; then
        local p
        for p in /usr/sbin/tuwunel /usr/bin/tuwunel /usr/local/bin/tuwunel; do
            [ -x "$p" ] && TUWUNEL="$p" && break
        done
    fi
    [ -n "$TUWUNEL" ] || die "tuwunel binary not found"
}

# Tuwunel configuration, rewritten on every boot so the lockdown cannot
# drift: federation is disabled entirely and registration only works with the
# local token, which doubles as the provisioning secret for the accounts.
write_tuwunel_config() {
    mkdir -p "$MATRIX_DIR/db"
    chmod 700 "$MATRIX_DIR"
    HS_CONFIG="$MATRIX_DIR/tuwunel.toml"
    REG_SECRET_FILE="$MATRIX_DIR/registration.secret"

    if [ ! -f "$REG_SECRET_FILE" ]; then
        (
            umask 077
            gen_secret >"$REG_SECRET_FILE"
        )
    fi
    REG_SECRET="$(cat "$REG_SECRET_FILE")"

    (
        umask 077
        cat >"$HS_CONFIG" <<EOF
# Written by the lily sandbox entrypoint on every boot — edits do not persist.
[global]
server_name = "$SERVER_NAME"
database_path = "$MATRIX_DIR/db"
address = "0.0.0.0"
port = 8008
allow_federation = false
allow_registration = true
registration_token = "$REG_SECRET"
trusted_servers = []
allow_check_for_updates = false
EOF
    )
}

start_matrix() {
    find_tuwunel
    write_tuwunel_config
    if wait_http "$HS_URL/_matrix/client/versions" 1; then
        log "Tuwunel already running"
        return
    fi
    log "starting Tuwunel"
    supervise tuwunel "$LOG_DIR/tuwunel.log" "$TUWUNEL" -c "$HS_CONFIG" &
    wait_http "$HS_URL/_matrix/client/versions" 90 \
        || die "Tuwunel did not come up; see $LOG_DIR/tuwunel.log"
}

# Register an account through the token-gated UIAA flow. Some servers want a
# final m.login.dummy stage after the token stage, so follow up if asked.
register_user() { # localpart password
    local localpart="$1" password="$2" session resp i=0
    # Retry the initial probe: /versions can answer before /register is ready.
    while [ $i -lt 10 ]; do
        session="$(curl -s -X POST "$HS_URL/_matrix/client/v3/register" \
            -H 'Content-Type: application/json' -d '{}' | jq -r '.session // empty' || true)"
        [ -n "$session" ] && break
        i=$((i + 1))
        sleep 1
    done
    [ -n "$session" ] || die "homeserver did not open a registration session"
    resp="$(jq -n --arg u "$localpart" --arg p "$password" --arg t "$REG_SECRET" --arg s "$session" \
        '{username: $u, password: $p, inhibit_login: true,
          initial_device_display_name: "lily",
          auth: {type: "m.login.registration_token", token: $t, session: $s}}' \
        | curl -s -X POST "$HS_URL/_matrix/client/v3/register" \
            -H 'Content-Type: application/json' -d @-)"
    if jq -e '.completed and (.user_id | not)' >/dev/null 2>&1 <<<"$resp"; then
        resp="$(jq -n --arg u "$localpart" --arg p "$password" --arg s "$session" \
            '{username: $u, password: $p, inhibit_login: true,
              initial_device_display_name: "lily",
              auth: {type: "m.login.dummy", session: $s}}' \
            | curl -s -X POST "$HS_URL/_matrix/client/v3/register" \
                -H 'Content-Type: application/json' -d @-)"
    fi
    jq -e '.user_id' >/dev/null 2>&1 <<<"$resp" \
        || die "failed to register @$localpart:$SERVER_NAME — $resp"
}

# Password-login probe; the throwaway token is logged out again.
can_login() { # mxid password
    local token
    token="$(jq -n --arg u "$1" --arg p "$2" \
        '{type: "m.login.password", identifier: {type: "m.id.user", user: $u}, password: $p}' \
        | curl -s --max-time 10 -X POST "$HS_URL/_matrix/client/v3/login" -d @- \
        | jq -r '.access_token // empty')"
    [ -n "$token" ] || return 1
    curl -s --max-time 10 -o /dev/null -X POST -H "Authorization: Bearer $token" \
        "$HS_URL/_matrix/client/v3/logout"
}

# First boot creates the owner and bot accounts; afterwards the generated
# passwords are reused from credentials.env. Exports the Matrix settings
# lily reads.
ensure_matrix_accounts() {
    CRED_FILE="$SANDBOX_DIR/credentials.env"
    if [ ! -f "$CRED_FILE" ]; then
        log "first run: creating Matrix accounts $OWNER_MXID and $BOT_MXID"
        local bot_password owner_password
        bot_password="$(gen_secret)"
        owner_password="$(gen_secret)"
        # Owner first: Tuwunel grants server admin to the first registration.
        register_user "$OWNER_LOCALPART" "$owner_password"
        register_user "$BOT_LOCALPART" "$bot_password"
        (
            umask 077
            cat >"$CRED_FILE" <<EOF
MATRIX_OWNER_MXID=$OWNER_MXID
MATRIX_OWNER_PASSWORD=$owner_password
MATRIX_BOT_MXID=$BOT_MXID
MATRIX_BOT_PASSWORD=$bot_password
EOF
        )
    fi
    # Re-tighten permissions in case they drifted (the file is host-editable
    # on the shared mount, where chmod support can vary — warn, don't die).
    chmod 600 "$CRED_FILE" 2>/dev/null \
        || log "WARNING: could not tighten permissions on $CRED_FILE"
    # shellcheck disable=SC1090
    . "$CRED_FILE"

    # credentials.env and the database can drift apart — the database lives
    # on the sandbox disk, so recreating the sandbox starts it empty while
    # the stored passwords survive on the shared mount. Re-register missing
    # accounts with their stored passwords (an existing account with a wrong
    # password still dies in register_user, which is the right failure).
    if ! can_login "$OWNER_MXID" "$MATRIX_OWNER_PASSWORD"; then
        log "$OWNER_MXID missing from the homeserver — re-registering"
        register_user "$OWNER_LOCALPART" "$MATRIX_OWNER_PASSWORD"
    fi
    if ! can_login "$BOT_MXID" "$MATRIX_BOT_PASSWORD"; then
        log "$BOT_MXID missing from the homeserver — re-registering"
        register_user "$BOT_LOCALPART" "$MATRIX_BOT_PASSWORD"
    fi

    export MATRIX_HOMESERVER="$HS_URL"
    export MATRIX_USER="$MATRIX_BOT_MXID"
    export MATRIX_PASSWORD="$MATRIX_BOT_PASSWORD"
    # Only the owner may drive the bot unless config.env widens this explicitly.
    export LILY_ALLOWED_USERS="${LILY_ALLOWED_USERS:-$MATRIX_OWNER_MXID}"
}
