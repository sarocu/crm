#!/bin/sh
# Start Meilisearch inside a heyo microVM, with all state on /workspace.
#
# app-lb runs this as the deployment's `start_command`. It must return, so
# Meilisearch is daemonized and its output goes to a log file that
# `applb_exec` can read — the start_command's own output is not shipped
# anywhere.
#
# /workspace is the deployment's persistent workspace: app-lb captures it
# when the VM retires and seeds the next VM from it. The database, dumps,
# snapshots and the master key all live there, so they survive restarts,
# rebuilds and rollouts.
#
# The master key comes from MEILI_MASTER_KEY when the spec provides one
# (preferably through `env_from`). Otherwise one is generated on first boot
# and kept in /workspace/master.key, so it never appears in the spec.

set -eu

WS="${MEILI_WORKSPACE:-/workspace}"
LOG=/var/log/meilisearch.log

mkdir -p "$WS/data.ms" "$WS/dumps" "$WS/snapshots"

if [ -z "${MEILI_MASTER_KEY:-}" ]; then
    if [ ! -s "$WS/master.key" ]; then
        head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$WS/master.key"
        chmod 600 "$WS/master.key"
        echo "meili-start: generated a master key in $WS/master.key" >> "$LOG"
    fi
    MEILI_MASTER_KEY="$(cat "$WS/master.key")"
fi
export MEILI_MASTER_KEY

echo "meili-start: starting $(meilisearch --version 2>/dev/null) with db $WS/data.ms" >> "$LOG"
setsid nohup meilisearch \
    --db-path "$WS/data.ms" \
    --dump-dir "$WS/dumps" \
    --snapshot-dir "$WS/snapshots" \
    --http-addr "${MEILI_HTTP_ADDR:-0.0.0.0:7700}" \
    --env production \
    --no-analytics \
    --max-indexing-memory "${MEILI_MAX_INDEXING_MEMORY:-1GiB}" \
    </dev/null >>"$LOG" 2>&1 &

# Report a failure to come up here, where `applb_exec` will find it, rather
# than leaving an unhealthy pool with nothing to say.
i=0
while [ $i -lt 30 ]; do
    if curl -fsS -m 2 "http://127.0.0.1:7700/health" >/dev/null 2>&1; then
        echo "meili-start: healthy" >> "$LOG"
        exit 0
    fi
    i=$((i + 1))
    sleep 1
done
echo "meili-start: not healthy after 30s; see $LOG" >> "$LOG"
exit 0
