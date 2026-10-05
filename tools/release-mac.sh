#!/bin/sh
# Builds the macOS release file (tablet app built in) on this Mac and uploads it to the
# `latest` release next to the Linux one from CI. Needs Xcode 26, the Android SDK/NDK and gh.
set -eu
cd "$(dirname "$0")/.."
(cd android && ./gradlew -q assembleRelease)
apk=$PWD/android/app/build/outputs/apk/release/app-release.apk
TABDISPLAY_APK=$apk cargo build --release -p tabdisplay-host
cp target/release/tabdisplay-host target/tabdisplay-macos-arm64
gh release view latest >/dev/null 2>&1 || gh release create latest --prerelease --title "TabDisplay (latest)" \
    --notes "Built from main. Download the file for your computer and run it; see the README."
gh release upload latest target/tabdisplay-macos-arm64 --clobber
echo "uploaded tabdisplay-macos-arm64 ($(du -h target/tabdisplay-macos-arm64 | cut -f1))"
