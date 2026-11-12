#!/bin/sh
# Builds the Unraid plugin: dist/inventoryx-VERSION.tgz and dist/inventoryx.plg.
# VERSION defaults to today's date (Unraid's usual YYYY.MM.DD form).
set -eu
cd "$(dirname "$0")/.."
VERSION="${1:-$(date +%Y.%m.%d)}"
./scripts/build.sh >/dev/null
STAGE=target/plugin-stage
mkdir -p "$STAGE/scripts" dist
cp target/x86_64-unknown-linux-musl/release/inventoryx "$STAGE/inventoryx"
cp plugin/api.php plugin/InventoryX.page plugin/InventoryXSettings.page plugin/default.cfg plugin/README.md "$STAGE/"
cp plugin/scripts/start plugin/scripts/stop "$STAGE/scripts/"
chmod 755 "$STAGE/inventoryx" "$STAGE/scripts/start" "$STAGE/scripts/stop"
BUNDLE="dist/inventoryx-$VERSION.tgz"
# No macOS metadata, owned by root.
COPYFILE_DISABLE=1 tar --no-xattrs --uid 0 --gid 0 --uname root --gname root -czf "$BUNDLE" -C "$STAGE" \
  inventoryx api.php InventoryX.page InventoryXSettings.page default.cfg README.md scripts
MD5=$( (md5sum "$BUNDLE" 2>/dev/null || md5 -r "$BUNDLE") | cut -d' ' -f1)
# The plugin file lives in the repository root: Unraid reads it from the main branch.
sed -e "s/@VERSION@/$VERSION/g" -e "s/@MD5@/$MD5/g" plugin/inventoryx.plg.in > inventoryx.plg
echo "$BUNDLE  md5 $MD5"
echo "inventoryx.plg"
