#!/usr/bin/env bash
# Builds both files on this Mac, each with the tablet app built in, into dist/ (or the folder
# given): nothing is committed, versioned or uploaded.
#
#   tabdisplay-macos-arm64    natively (needs Xcode 26)
#   tabdisplay-linux-x86_64   in Docker: x86-64 Ubuntu 24.04 with GStreamer bundled into the file,
#                             then run on clean Ubuntu and Arch containers against a stand-in tablet
#   tabdisplay.apk            the tablet app
#   SHA256SUMS
#
# Copy the Linux file to the Linux computer (scp, USB stick) and run it: nothing to install.
# tools/release.sh uses this for releases.
set -euo pipefail
cd "$(dirname "$0")/.."
out=${1:-dist}
die() { echo "$@" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }
command -v docker >/dev/null && docker info >/dev/null 2>&1 || die "the Linux build needs Docker running"

rm -rf "$out"
mkdir -p "$out"

step "tests"
cargo test -q --release -p tabdisplay-host 2>&1 | grep -E "test result|FAILED|panicked" || true
cargo test -q --release -p tabdisplay-host >/dev/null 2>&1 || die "cargo test failed"

step "tablet app"
(cd android && ./gradlew -q assembleRelease)
cp android/app/build/outputs/apk/release/app-release.apk "$out/tabdisplay.apk"
apk=$PWD/$out/tabdisplay.apk

step "macOS (arm64)"
TABDISPLAY_APK=$apk cargo build -q --release -p tabdisplay-host
cp target/release/tabdisplay-host "$out/tabdisplay-macos-arm64"

step "Linux (x86-64, in Docker)"
docker build -q --platform linux/amd64 -t tabdisplay-build-amd64 -f tools/linux-build.Dockerfile tools >/dev/null
docker run --rm --platform linux/amd64 -v "$PWD":/src -w /src \
  -v tabdisplay-cargo-noble:/root/.cargo/registry -v tabdisplay-target-noble:/target -e CARGO_TARGET_DIR=/target \
  -e OUT="$out" tabdisplay-build-amd64 sh -c '
    set -e
    TABDISPLAY_APK=/src/$OUT/tabdisplay.apk cargo build -q --release -p tabdisplay-host
    tools/bundle-linux.sh /target/release/tabdisplay-host /target/payload.tar.zst
    TD_PAYLOAD=/target/payload.tar.zst cargo build -q --release -p tabdisplay-launcher
    cp /target/release/tabdisplay /src/$OUT/tabdisplay-linux-x86_64'

# On systems without GStreamer the file has to bring everything it needs.
for image in ubuntu:24.04 archlinux:latest; do
  step "Linux file on a clean $image"
  docker run --rm --platform linux/amd64 -v "$PWD":/src:ro -e OUT="$out" "$image" sh -c '
    if command -v pacman >/dev/null; then pacman -Sy --noconfirm --disable-sandbox python >/dev/null 2>&1
    else apt-get update -qq && apt-get install -y -qq python3 >/dev/null 2>&1; fi
    cp /src/$OUT/tabdisplay-linux-x86_64 /tmp/td
    /tmp/td --no-adb --test-source --fps 60 >/tmp/host.log 2>&1 &
    sleep 5
    W=1280 H=800 TILES=1 timeout 20 python3 /src/tools/fake_tablet.py >/tmp/fake.log 2>&1
    kill -INT $! 2>/dev/null; sleep 1
    grep -E "tiles=|frames=" /tmp/fake.log
    grep -q "all unpacked" /tmp/fake.log && grep -q "keyframes_at=\[" /tmp/fake.log || { cat /tmp/fake.log /tmp/host.log; exit 1; }' ||
    die "the Linux file failed on $image"
done

(cd "$out" && shasum -a 256 tabdisplay-macos-arm64 tabdisplay-linux-x86_64 tabdisplay.apk > SHA256SUMS)
step "built into $out/"
ls -lh "$out" | tail -n +2
