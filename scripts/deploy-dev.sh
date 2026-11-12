#!/bin/sh
# Development install onto an Unraid server. Not persistent: Unraid's web UI folder lives
# in memory, so this is gone after a reboot.
#
# Touches only InventoryX's own files:
#   /usr/local/emhttp/plugins/inventoryx/   program, page and forwarder
#   /var/run/inventoryx.sock, .log          socket and log
# Nothing under /mnt is written. This build contains no delete code.
set -eu
HOST="${1:-root@tower.local}"
cd "$(dirname "$0")/.."
DIR=/usr/local/emhttp/plugins/inventoryx
BIN=target/x86_64-unknown-linux-musl/release/inventoryx

ssh "$HOST" "mkdir -p $DIR"
scp -q "$BIN" "$HOST:$DIR/inventoryx.new"
scp -q plugin/api.php plugin/InventoryX.page "$HOST:$DIR/"
ssh "$HOST" "
  set -e
  pkill -x inventoryx || true
  sleep 0.5
  mv -f $DIR/inventoryx.new $DIR/inventoryx
  chmod 755 $DIR/inventoryx
  rm -f /var/run/inventoryx.sock
  nohup $DIR/inventoryx serve --socket /var/run/inventoryx.sock >/var/run/inventoryx.log 2>&1 </dev/null &
  sleep 1
  pidof inventoryx >/dev/null && echo 'InventoryX is running' || { echo 'InventoryX failed to start'; cat /var/run/inventoryx.log; exit 1; }
"
