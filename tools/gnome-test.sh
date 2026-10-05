#!/bin/sh
# Runs dist/tabdisplay-linux-x86_64 (tools/build.sh) against GNOME's compositor in Docker: a
# portal-style remote desktop + virtual monitor session, pointer moves, frames to a stand-in tablet.
set -eu
cd "$(dirname "$0")/.."
docker build -q --platform linux/amd64 -t tabdisplay-gnome-test tools/gnome-test >/dev/null
docker run --rm --platform linux/amd64 -v "$PWD":/src:ro -v "$PWD/tools/gnome-test":/test:ro tabdisplay-gnome-test \
    dbus-run-session -- sh /test/run.sh && echo "GNOME test passed"
