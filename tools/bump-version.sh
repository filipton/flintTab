#!/usr/bin/env bash
# Moves every place that names the release to a new version, in one go:
#   tools/bump-version.sh 0.3.1
#
#   host, launcher, vdisplay-ffi, android/native   Cargo.toml version (and the lock files' entries)
#   android/app/build.gradle.kts                   versionName, and versionCode as
#                                                  major*10000 + minor*100 + patch (0.3.1 -> 301)
# Prints what changed; commits nothing.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
new="${1:-}"
[[ "$new" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] || { echo "usage: tools/bump-version.sh <major.minor.patch>" >&2; exit 2; }
code=$(( BASH_REMATCH[1] * 10000 + BASH_REMATCH[2] * 100 + BASH_REMATCH[3] ))
old=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/host/Cargo.toml" | head -1)
[ "$old" != "$new" ] || { echo "already at $new" >&2; exit 1; }
echo "$old -> $new (versionCode $code)"

python3 - "$root" "$new" "$code" <<'PY'
import re, sys, pathlib
root, new, code = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]

def edit(path, subs):
    p = root / path
    text = p.read_text(); before = text
    for pattern, repl in subs:
        text = re.sub(pattern, repl, text, flags=re.M)
    if text != before:
        p.write_text(text); print(f"  {path}")
    else:
        print(f"  {path}: nothing to change")

crates = {"host": "tabdisplay-host", "launcher": "tabdisplay-launcher", "vdisplay-ffi": "vdisplay-ffi",
          "android/native": "tabdisplay-native"}
for d in crates:
    edit(f"{d}/Cargo.toml", [(r'^(\[package\][^\[]*?\nversion = )"[^"]+"', rf'\g<1>"{new}"')])
lock = lambda names: [(rf'(\[\[package\]\]\nname = "{re.escape(n)}"\nversion = )"[^"]+"', rf'\g<1>"{new}"') for n in names]
edit("Cargo.lock", lock(["tabdisplay-host", "tabdisplay-launcher", "vdisplay-ffi"]))
if (root / "android/native/Cargo.lock").exists():
    edit("android/native/Cargo.lock", lock(["tabdisplay-native"]))
edit("android/app/build.gradle.kts", [
    (r'versionName = "[^"]+"', f'versionName = "{new}"'),
    (r'versionCode = \d+', f'versionCode = {code}'),
])
PY
