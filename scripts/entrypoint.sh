#!/usr/bin/env bash
# =============================================================================
# mvp-keeper-bot — runtime entrypoint
# =============================================================================
# Handles the Railway-friendly KEYPAIR_BASE64 pattern for admin keypair
# delivery without needing filesystem secret mounts.
#
# Behaviour:
#   1. If KEYPAIR_BASE64 is set → base64-decode to /tmp/admin.json (0600),
#      export KEYPAIR=/tmp/admin.json, proceed.
#   2. Else if KEYPAIR is set and points at an existing file → use as-is
#      (local dev with docker bind-mounted keypair).
#   3. Else → exit 1 with actionable error.
#
# Generates KEYPAIR_BASE64 locally with:
#   base64 -i ~/.config/solana/id.json | tr -d '\n'
#
# On Railway: paste the base64 string into KEYPAIR_BASE64 via the service
# variables pane (use Shared Variables so api + keeper stay in sync).
# =============================================================================

set -euo pipefail

if [ -n "${KEYPAIR_BASE64:-}" ]; then
    # base64 output may or may not include trailing newlines; decode handles both.
    printf '%s' "$KEYPAIR_BASE64" | base64 -d > /tmp/admin.json
    chmod 600 /tmp/admin.json
    export KEYPAIR=/tmp/admin.json
elif [ -z "${KEYPAIR:-}" ]; then
    echo "ERROR: No keypair available." >&2
    echo "Set KEYPAIR_BASE64 (Railway/Vercel) or KEYPAIR (path to file for local dev)." >&2
    exit 1
elif [ ! -f "$KEYPAIR" ]; then
    echo "ERROR: KEYPAIR points to missing file: $KEYPAIR" >&2
    exit 1
fi

# Hand off to the real binary. "$@" carries the CMD (serve-api|run-crons|full|start-round).
exec mvp-keeper-bot "$@"
