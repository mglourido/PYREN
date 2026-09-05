#!/bin/sh
# bump-version.sh - set Pyren's version in the five places it is written.
#
# The daemon workspace, the app (package.json and tauri.conf.json), the
# Tauri shell crate and the widget each carry the version string, and a
# release build refuses to run unless they all agree (tools/release.sh
# checks). This is the one command that moves them together.
#
#   tools/bump-version.sh 0.2.0     set every manifest to 0.2.0
#   tools/bump-version.sh --show    print what each file currently says
#
# It edits files and refreshes the three Cargo.lock files. It does NOT
# commit, tag or push - see install/INSTALL.md for the release steps.

set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

# name -> file, and the sed program that rewrites the version in it. The
# Cargo manifests anchor on a line-leading `version = "..."` so the
# `version = "1"` inside dependency tables is left alone.
DAEMON_TOML="$ROOT/daemon/Cargo.toml"
OSD_TOML="$ROOT/osd/Cargo.toml"
TAURI_TOML="$ROOT/app/src-tauri/Cargo.toml"
APP_PKG="$ROOT/app/package.json"
TAURI_CONF="$ROOT/app/src-tauri/tauri.conf.json"

current() {
    # First line-leading version in a Cargo.toml, or the "version" key in a
    # JSON file - both unique to the package/workspace metadata here.
    case $1 in
    *.toml) sed -n 's/^version = "\([^"]*\)".*/\1/p' "$1" | head -n1 ;;
    *.json) sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$1" | head -n1 ;;
    esac
}

show() {
    for f in "$DAEMON_TOML" "$OSD_TOML" "$TAURI_TOML" "$APP_PKG" "$TAURI_CONF"; do
        printf '%-40s %s\n' "${f#"$ROOT"/}" "$(current "$f")"
    done
}

case "${1:-}" in
-h | --help)
    sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
--show)
    show
    exit 0
    ;;
"")
    echo "bump-version.sh: need a version (e.g. 0.2.0) or --show" >&2
    exit 2
    ;;
esac

VERSION=$1
if ! printf '%s' "$VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "bump-version.sh: '$VERSION' is not X.Y.Z" >&2
    exit 2
fi

# `.bak` then delete, so this works the same under GNU and BSD sed.
edit() {
    sed -i.bak "$2" "$1" && rm -f "$1.bak"
    echo "  ${1#"$ROOT"/}  ->  $VERSION"
}

echo "Setting version to $VERSION:"
# Each manifest has exactly one line-leading `version = "..."` (the package
# or workspace one); dependency tables write it as `{ version = "1", ... }`,
# never at the start of a line, so `^` is enough to tell them apart.
edit "$DAEMON_TOML" "s/^version = \".*\"/version = \"$VERSION\"/"
edit "$OSD_TOML" "s/^version = \".*\"/version = \"$VERSION\"/"
edit "$TAURI_TOML" "s/^version = \".*\"/version = \"$VERSION\"/"
edit "$APP_PKG" "s/\"version\": *\"[^\"]*\"/\"version\": \"$VERSION\"/"
edit "$TAURI_CONF" "s/\"version\": *\"[^\"]*\"/\"version\": \"$VERSION\"/"

# Refresh the workspace-member versions in each lock file. --workspace
# touches only path members, never registry dependencies.
for ws in daemon osd app/src-tauri; do
    echo "  cargo update --workspace ($ws)"
    (cd "$ROOT/$ws" && cargo update --workspace >/dev/null 2>&1) ||
        echo "    note: could not refresh $ws/Cargo.lock - run 'cargo update --workspace' there yourself"
done

echo
show
echo
echo "Next: add a CHANGELOG.md entry, commit, then tools/release.sh"
