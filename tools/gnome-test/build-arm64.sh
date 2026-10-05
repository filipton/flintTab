#!/bin/sh
# The arm64 test build of the Linux file (tools/gnome-test.sh), into target/test/.
set -e
cd "$(dirname "$0")/../.."
docker build -q -t tabdisplay-build-arm64 -f tools/linux-build.Dockerfile tools >/dev/null
docker run --rm -v "$PWD":/src -w /src -v tabdisplay-cargo-arm64:/root/.cargo/registry \
    -v tabdisplay-target-arm64:/target -e CARGO_TARGET_DIR=/target tabdisplay-build-arm64 sh -c '
        set -e
        cargo build -q --release -p tabdisplay-host 2>&1 | grep -E "^error" -A8 | head -30
        tools/bundle-linux.sh /target/release/tabdisplay-host /target/payload.tar.zst >/dev/null
        TD_PAYLOAD=/target/payload.tar.zst cargo build -q --release -p tabdisplay-launcher
        mkdir -p /src/target/test && cp /target/release/tabdisplay /src/target/test/tabdisplay-linux-arm64'
