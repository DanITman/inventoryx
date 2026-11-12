#!/bin/sh
# Fills in the GitHub account name everywhere it is needed.
set -eu
[ $# -eq 1 ] || { echo "usage: $0 GITHUB_USERNAME" >&2; exit 2; }
cd "$(dirname "$0")/.."
for f in README.md ca_profile.xml plugins/inventoryx.xml plugin/inventoryx.plg.in; do
  sed -i.bak -e "s/@OWNER@/$1/g" "$f" && rm -f "$f.bak"
done
echo "Owner set to $1. Run ./scripts/package.sh to regenerate inventoryx.plg."
