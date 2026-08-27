#!/usr/bin/env bash
# Idempotent Cloud Agent bootstrap for the durable Rust workspace.
set -euo pipefail

cd "$(dirname "$0")/.."

# durable depends on the `rocksdb` crate, which compiles RocksDB from C++ source.
# The base image exposes clang as the default `c++`, but ships only the
# libstdc++-13 headers while its runtime library is libstdc++-14. Clang's GCC
# toolchain detection then targets the v14 install directory whose C++ headers
# are missing, so every RocksDB translation unit fails with "'memory' file not
# found". Install the matching headers so the default compiler can build RocksDB.
if ! printf '#include <memory>\nint main(){return 0;}\n' | c++ -x c++ - -o /dev/null 2>/dev/null; then
  sudo apt-get update -qq
  sudo apt-get install -y --no-install-recommends libstdc++-14-dev
fi

# The committed Cargo.lock pins crates that require the Rust 2024 edition
# (stabilized in Rust 1.85). The base image defaults to 1.83, so make a recent
# stable toolchain the default before building.
rustup toolchain install stable --profile minimal --no-self-update
rustup default stable

# Warm the build (and RocksDB compile) so the workspace is ready to test/run.
cargo build --workspace --locked
