# banner.sh — host-visible conveniences: environment for `sbx exec` shells
# and the connection banner printed once the stack is up.

# Make `sbx exec -it <name> bash` shells pick up the stack's environment so
# the lily CLI (project/task/send) works against the right data dir.
write_shell_env() {
    (
        umask 077
        {
            printf 'export LILY_DATA_DIR=%q\n' "$LILY_DATA_DIR"
            printf 'export OPENCODE_URL=%q\n' "$OPENCODE_URL"
            printf 'export MATRIX_HOMESERVER=%q\n' "$MATRIX_HOMESERVER"
        } >"$HOME/.lily-env"
    )
    if ! grep -qs 'lily-env' "$HOME/.bashrc"; then
        printf '\n[ -f "$HOME/.lily-env" ] && . "$HOME/.lily-env"\n' \
            >>"$HOME/.bashrc"
    fi
}

print_banner() {
    cat <<EOF

==============================================================================
 lily sandbox is up
   Matrix server   $SERVER_NAME   (Tuwunel — federation OFF, registration token-gated)
   Allowed driver  $LILY_ALLOWED_USERS
   Your login      $MATRIX_OWNER_MXID
   Your password   $SANDBOX_DIR/credentials.env  (readable from the host)
   Bot account     $BOT_MXID
   Remote URL      ${NGROK_URL:-— (set NGROK_AUTHTOKEN in config.env to enable)}

 Connect a Matrix client:
   local:  on the host run    sbx ports <sandbox-name> --publish 8008:8008
           then use http://localhost:8008 as the homeserver URL
   remote: use ${NGROK_URL:-the ngrok URL} as the homeserver URL
   log in as $MATRIX_OWNER_MXID, create a room, invite $BOT_MXID
   (it auto-joins), then link a mounted project: !add-project /abs/path
==============================================================================

EOF
}
