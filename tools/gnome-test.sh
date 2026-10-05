#!/bin/sh
# Runs the Linux host against GNOME Shell in Docker, in a portal-style remote desktop +
# virtual-monitor screencast, with a stand-in tablet: frames, empty "cursor only" frames, pointer.
# GNOME Shell runs headless under systemd (logind) in Fedora, natively on this Mac's CPU, so the
# host is built for arm64 here (same code and bundling as the x86-64 release file).
#   CURSOR=2 (cursor as metadata: mutter's empty frames)   STILL=--still (no pointer moves)
#   COMPOSITOR=mutter (bare mutter instead of GNOME Shell)
# Not covered: what a real GPU does differently (Docker on a Mac has none).
set -eu
cd "$(dirname "$0")/.."
docker build -q -t tabdisplay-build-arm64 -f tools/linux-build.Dockerfile tools >/dev/null
docker run --rm -v "$PWD":/src -w /src -v tabdisplay-cargo-arm64:/root/.cargo/registry \
    -v tabdisplay-target-arm64:/target -e CARGO_TARGET_DIR=/target tabdisplay-build-arm64 sh -c '
        set -e
        cargo build -q --release -p tabdisplay-host
        tools/bundle-linux.sh /target/release/tabdisplay-host /target/payload.tar.zst >/dev/null
        TD_PAYLOAD=/target/payload.tar.zst cargo build -q --release -p tabdisplay-launcher
        mkdir -p /src/target/test && cp /target/release/tabdisplay /src/target/test/tabdisplay-linux-arm64'
docker build -q -t tabdisplay-gnome-test tools/gnome-test >/dev/null
docker rm -f tabdisplay-gnome >/dev/null 2>&1 || true
docker run -d -t --name tabdisplay-gnome --privileged --cgroupns=private -e container=docker \
    --tmpfs /run --tmpfs /run/lock --tmpfs /tmp -v "$PWD":/src:ro -v "$PWD/tools/gnome-test":/test:ro \
    tabdisplay-gnome-test >/dev/null
trap 'docker rm -f tabdisplay-gnome >/dev/null 2>&1' EXIT
sleep 5
docker exec -e BIN=/src/target/test/tabdisplay-linux-arm64 -e CURSOR="${CURSOR:-1}" -e STILL="${STILL:-}" \
    -e COMPOSITOR="${COMPOSITOR:-gnome-shell}" tabdisplay-gnome dbus-run-session -- sh /test/run.sh 2>/dev/null |
    grep -E "tiles=|frames=|cannot be read|capture failed|no picture|CRITICAL|latency|pointer" &&
    echo "GNOME test passed"
