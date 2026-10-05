#!/usr/bin/env bash
# Makes a release on this Mac and puts it on GitHub:
#
#   tools/release.sh 0.3.1            bump to 0.3.1, changelog, build, test, tag, push, release
#   tools/release.sh 0.3.1 --draft    the GitHub release as a draft
#   tools/release.sh 0.3.1 --build    only bump and build, into target/release-0.3.1/; nothing is
#                                     committed or leaves the machine
#   tools/release.sh 0.3.1 --yes      no questions
#
# Steps: tools/bump-version.sh, then CHANGELOG.md's Unreleased section (generated from the commits
# since the last tag by tools/changelog.py when empty) becomes this version's; the notes are shown
# to accept, edit ($EDITOR) or abandon (every file put back). Then "build: release <version>" is
# committed and the files are built:
#
#   tabdisplay-macos-arm64    natively (needs Xcode 26)
#   tabdisplay-linux-x86_64   in Docker: x86-64 Debian 12 with GStreamer bundled into the file,
#                             then run on clean Ubuntu and Arch containers against a stand-in tablet
#   tabdisplay.apk            the tablet app (also built into both of the above)
#   SHA256SUMS
#
# then tagged v<version>, pushed, and attached to a GitHub release with the changelog section as
# notes. If a build fails after the commit, run the same command again: it carries on from there.
# The APK is signed with this Mac's Android debug key, as every build so far: a tablet updates in
# place only from the same key.
set -euo pipefail
cd "$(dirname "$0")/.."

version=""
BUILD_ONLY=0
DRAFT=""
YES=0
for a in "$@"; do
  case "$a" in
    --build) BUILD_ONLY=1 ;;
    --draft) DRAFT=--draft ;;
    --yes) YES=1 ;;
    -h|--help) sed -n 2,27p "$0"; exit 0 ;;
    -*) echo "unknown option: $a" >&2; exit 2 ;;
    *) version=$a ;;
  esac
done

die() { echo "$@" >&2; exit 1; }
# What a release commit holds (and "no" puts back), nothing else.
RELEASE_FILES=(host/Cargo.toml launcher/Cargo.toml vdisplay-ffi/Cargo.toml android/native/Cargo.toml Cargo.lock
               android/native/Cargo.lock android/app/build.gradle.kts CHANGELOG.md)
step() { printf '\n==> %s\n' "$*"; }
code_version() { sed -n 's/^version = "\(.*\)"/\1/p' host/Cargo.toml | head -1; }
newer() { [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" = "$1" ]; }

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "usage: tools/release.sh <major.minor.patch> [--draft|--build|--yes]"
last=$(git tag --list 'v*' --sort=-v:refname | head -1)
last=${last#v}
echo "last release: ${last:-none yet}   in the code: $(code_version)   new: $version"
[ -z "$last" ] || newer "$version" "$last" || die "$version is not after the last release ($last)"
git rev-parse -q --verify "refs/tags/v$version" >/dev/null && die "v$version is already tagged"

if [ $BUILD_ONLY = 0 ]; then
  command -v gh >/dev/null || die "releasing needs the GitHub CLI (gh)"
  gh auth status >/dev/null 2>&1 || die "gh is not logged in: run 'gh auth login'"
  [ -z "$(git status --porcelain)" ] || die "commit or stash your changes first: the release commit holds only the release"
fi
command -v docker >/dev/null && docker info >/dev/null 2>&1 || die "the Linux build needs Docker running"

# --- version, changelog, commit (skipped when re-run after a failed build) ------------------------
if [ "$(code_version)" = "$version" ] && git log -1 --format=%s | grep -qx "build: release $version"; then
  echo "carrying on with the release commit already made for $version"
else
  step "version $version"
  tools/bump-version.sh "$version"
  if ! tools/changelog.py --notes "$version" >/dev/null 2>&1; then
    # Unreleased as written by hand, else generated from the commits since the last release.
    unreleased=$(awk '/^## \[Unreleased\]/{on=1;next} /^## /{on=0} on' CHANGELOG.md | grep -v '^\s*$' || true)
    [ -n "$unreleased" ] || tools/changelog.py --update
    tools/changelog.py --release "$version"
  fi
  step "release notes"
  tools/changelog.py --notes "$version"
  while [ $YES = 0 ]; do
    read -rp $'\n'"release $version with these notes? [y]es / [e]dit / [n]o: " answer
    case "$answer" in
      y|Y|"") break ;;
      e|E) "${EDITOR:-vi}" CHANGELOG.md; tools/changelog.py --notes "$version" ;;
      n|N) git checkout -- "${RELEASE_FILES[@]}" && echo "nothing changed" && exit 1 ;;
    esac
  done
  if [ $BUILD_ONLY = 0 ]; then
    git add -- "${RELEASE_FILES[@]}"
    git commit -qm "build: release $version"
  fi
fi

# --- build ----------------------------------------------------------------------------------------
out=target/release-$version
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
  -v tabdisplay-cargo-amd64:/usr/local/cargo/registry -v tabdisplay-target-amd64:/target -e CARGO_TARGET_DIR=/target \
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
    if command -v pacman >/dev/null; then pacman -Sy --noconfirm python >/dev/null 2>&1
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
step "built"
ls -lh "$out" | tail -n +2

if [ $BUILD_ONLY = 1 ]; then
  echo
  echo "built into $out/ (nothing committed; tools/bump-version.sh changed the version files)"
  exit 0
fi

# --- publish --------------------------------------------------------------------------------------
step "publishing v$version"
notes=$(mktemp)
trap 'rm -f "$notes"' EXIT
tools/changelog.py --notes "$version" > "$notes"
git tag -a "v$version" -m "TabDisplay $version"
# The branch first: a tag on a commit no branch has is one nobody reaches from the repo page.
git push -q origin "$(git branch --show-current)"
git push -q origin "v$version"
gh release create "v$version" "$out"/tabdisplay-macos-arm64 "$out"/tabdisplay-linux-x86_64 "$out"/tabdisplay.apk \
  "$out"/SHA256SUMS --title "TabDisplay $version" --notes-file "$notes" $DRAFT
echo
echo "done: $(gh release view "v$version" --json url --jq .url)"
