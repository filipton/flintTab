#!/usr/bin/env bash
# Makes a release on this Mac and puts it on GitHub:
#
#   tools/release.sh                  the guided release: shows the last release and the version in
#                                     the code, asks for the new version, the notes, draft or live
#   tools/release.sh 0.3.1            the same for 0.3.1, asking only about the notes
#   tools/release.sh 0.3.1 --draft    the GitHub release as a draft
#   tools/release.sh 0.3.1 --build    only bump and build, into target/release-0.3.1/; nothing is
#                                     committed or leaves the machine
#   tools/release.sh 0.3.1 --yes      no questions
#
# Steps: tools/bump-version.sh, then CHANGELOG.md's Unreleased section (generated from the commits
# since the last tag by tools/changelog.py when empty) becomes this version's; the notes are shown
# to accept, edit ($EDITOR) or abandon (every file put back). Then "build: release <version>" is
# committed, the files are built by tools/build.sh (both computers' files, the app, checksums),
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
    -h|--help) sed -n 2,29p "$0"; exit 0 ;;
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

last=$(git tag --list 'v*' --sort=-v:refname | head -1)
last=${last#v}
echo "last release:      ${last:-none yet}"
echo "version in code:   $(code_version)"
guided=0
if [ -z "$version" ]; then
  [ -t 0 ] || die "usage: tools/release.sh <major.minor.patch> [--draft|--build|--yes] (no version: asks, in a terminal)"
  guided=1
  base=${last:-$(code_version)}
  IFS=. read -r ma mi pa <<< "$base"
  # The code may already be ahead (a release commit made, the build failed): offer that first.
  suggest=$(code_version)
  [ -n "$last" ] && ! newer "$suggest" "$last" && suggest="$ma.$mi.$((pa + 1))"
  read -rp "new version [$suggest]: " version
  version=${version:-$suggest}
fi
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "not a version: $version (major.minor.patch)"
echo "new version:       $version"
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
  [ "$(code_version)" = "$version" ] || tools/bump-version.sh "$version"
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
tools/build.sh "$out"

if [ $BUILD_ONLY = 1 ]; then
  echo
  echo "built into $out/ (nothing committed; tools/bump-version.sh changed the version files)"
  exit 0
fi

# --- publish --------------------------------------------------------------------------------------
if [ $guided = 1 ] && [ -z "$DRAFT" ]; then
  read -rp $'\n'"publish v$version now? [l]ive / [d]raft / [n]ot yet: " answer
  case "$answer" in
    d|D) DRAFT=--draft ;;
    n|N) echo "built into $out/; the release commit is made but not pushed. Run tools/release.sh $version to publish."; exit 0 ;;
  esac
fi
step "publishing v$version${DRAFT:+ (draft)}"
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
