#!/bin/sh
# Builds the static Linux binary for Unraid (x86-64). Needs rustup with the
# x86_64-unknown-linux-musl target; links with the bundled rust-lld.
set -eu
cd "$(dirname "$0")/.."
cargo build --release --target x86_64-unknown-linux-musl
ls -l target/x86_64-unknown-linux-musl/release/inventoryx
