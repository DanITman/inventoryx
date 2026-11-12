#!/bin/sh
# Builds the static Linux binary for Unraid (x86-64). Needs rustup with the
# x86_64-unknown-linux-musl target; links with the bundled rust-lld.
set -eu
cd "$(dirname "$0")/.."
# Keep the builder's home folder (and so their user name) out of the binary.
export RUSTFLAGS="--remap-path-prefix=$HOME=/build ${RUSTFLAGS:-}"
cargo build --release --target x86_64-unknown-linux-musl
if grep -a -q -F "$HOME" target/x86_64-unknown-linux-musl/release/inventoryx; then
  echo "error: the binary still contains $HOME" >&2; exit 1
fi
ls -l target/x86_64-unknown-linux-musl/release/inventoryx
