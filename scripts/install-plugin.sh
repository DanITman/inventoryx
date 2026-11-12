#!/bin/sh
# Installs the packaged plugin on an Unraid server before it is published anywhere:
# puts the bundle on the flash drive where the .plg expects it, then runs Unraid's own
# plugin installer. After this the plugin survives reboots and shows on the Plugins tab.
set -eu
HOST="${1:-root@tower.local}"
cd "$(dirname "$0")/.."
BUNDLE=$(ls -t dist/inventoryx-*.tgz | head -1)
ssh "$HOST" "mkdir -p /boot/config/plugins/inventoryx"
scp -q "$BUNDLE" "$HOST:/boot/config/plugins/inventoryx/"
scp -q inventoryx.plg "$HOST:/tmp/inventoryx.plg"
ssh "$HOST" "plugin install /tmp/inventoryx.plg forced"
