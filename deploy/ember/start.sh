#!/bin/sh
set -eu

export HOP_WS_PATH="${HOP_WS_PATH:-/ws}"
DATA="${HOP_DATA_DIR:-/data/beings/hop-data}"
mkdir -p "$DATA"

if [ ! -f "$DATA/log.jsonl" ] && [ -f /data/beings/ember.jsonl ]; then
  echo "[ember] porting /data/beings/ember.jsonl into $DATA"
  python3 /app/port_ember_log.py /data/beings/ember.jsonl --data "$DATA"
fi

caddy run --config /app/Caddyfile --adapter caddyfile &
exec hopd /app/ember.hop 9000 9001 --data "$DATA" --web /app/hop-web/pkg
