#!/usr/bin/env bash
# Builds a release binary inside an Ubuntu 22.04 container so it links
# against glibc 2.35 and runs on any reasonably current Linux x86_64
# (Ubuntu 22.04+, Debian 12+). A binary built on a newer host would
# demand the host's newer glibc.
#
# Usage: scripts/build-release.sh   -> dist/parano1d-permanode-<version>-linux-x86_64.tar.gz
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(grep -m1 '^version' permanode/Cargo.toml | cut -d'"' -f2)
mkdir -p dist
docker run --rm \
  -v "$PWD":/src \
  -v parano1d-permanode-cargo:/root/.cargo/registry \
  -v parano1d-permanode-git:/root/.cargo/git \
  -w /src \
  ubuntu:22.04 bash -euo pipefail -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends curl ca-certificates build-essential clang libclang-dev pkg-config git >/dev/null
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null
    . "$HOME/.cargo/env"
    export CARGO_TARGET_DIR=/src/target/release-container
    cargo build --release --locked -p parano1d-permanode
    strip "$CARGO_TARGET_DIR/release/parano1d-permanode"
    cp "$CARGO_TARGET_DIR/release/parano1d-permanode" /src/dist/parano1d-permanode
    chown "$(stat -c %u:%g /src/README.md)" /src/dist/parano1d-permanode
  '
tar -C dist -czf "dist/parano1d-permanode-${version}-linux-x86_64.tar.gz" parano1d-permanode
rm dist/parano1d-permanode
ls -la dist/
