#!/bin/sh
# release.sh - build every part of Pyren, optimized, and pack one archive
# ready to attach to a GitHub Release.
#
# Pyren is three Cargo workspaces plus a frontend (see docs/02-development.md),
# and nothing wired them together for shipping. This does:
#
#   1. preflight  - tools present, tree clean, the five version strings agree,
#                   the GUI libraries the app and widget need are installed
#   2. checks     - the same tests + clippy + svelte-check CI runs
#   3. build      - cargo --release for the daemon and the widget; the app
#                   through `tauri build --no-bundle` (one self-contained ELF)
#   4. stage      - assemble the archive tree (bin/, share/, lib/, install.sh)
#   5. package    - pyren-<version>-x86_64-linux.tar.gz + SHA256SUMS in dist/
#
#   tools/release.sh                 the full run
#   tools/release.sh --skip-tests    trust a green CI, go straight to building
#   tools/release.sh --appimage      also build a portable .AppImage
#   tools/release.sh --publish       create a DRAFT GitHub release with gh
#   tools/release.sh --output DIR    where the archive goes (default dist/)
#
# It never tags, pushes or publishes anything but a draft. The release
# steps around it are in install/INSTALL.md ("Cutting a release").

set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

OUT="$ROOT/dist"
STAGE="$ROOT/build/stage"
run_tests=yes
appimage=no
publish=no
allow_dirty=no

while [ $# -gt 0 ]; do
    case "$1" in
    --output) OUT=$2; shift 2 ;;
    --skip-tests) run_tests=no; shift ;;
    --appimage) appimage=yes; shift ;;
    --publish) publish=yes; shift ;;
    --allow-dirty) allow_dirty=yes; shift ;;
    -h | --help)
        sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "release.sh: unknown argument '$1' (try --help)" >&2
        exit 2
        ;;
    esac
done

say() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
die() {
    echo "release.sh: $1" >&2
    exit 1
}

# --- 1. preflight --------------------------------------------------------

say "preflight"

for tool in cargo bun git tar sha256sum install pkg-config; do
    command -v "$tool" >/dev/null 2>&1 || die "'$tool' is not on PATH"
done
[ "$publish" = no ] || command -v gh >/dev/null 2>&1 || die "--publish needs the GitHub CLI (gh)"

if [ "$allow_dirty" = no ] && [ -n "$(git -C "$ROOT" status --porcelain)" ]; then
    die "working tree is not clean (commit first, or pass --allow-dirty)"
fi

# The version string, read from each manifest the way bump-version.sh writes it.
toml_version() { sed -n 's/^version = "\([^"]*\)".*/\1/p' "$1" | head -n1; }
json_version() { sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$1" | head -n1; }

VERSION=$(toml_version "$ROOT/daemon/Cargo.toml")
[ -n "$VERSION" ] || die "could not read the version from daemon/Cargo.toml"

check_version() {
    got=$2
    [ "$got" = "$VERSION" ] ||
        die "version mismatch: daemon/Cargo.toml says $VERSION but $1 says '${got:-nothing}'
     run  tools/bump-version.sh $VERSION  to line them up"
}
check_version "osd/Cargo.toml" "$(toml_version "$ROOT/osd/Cargo.toml")"
check_version "app/src-tauri/Cargo.toml" "$(toml_version "$ROOT/app/src-tauri/Cargo.toml")"
check_version "app/package.json" "$(json_version "$ROOT/app/package.json")"
check_version "app/src-tauri/tauri.conf.json" "$(json_version "$ROOT/app/src-tauri/tauri.conf.json")"

echo "  version   $VERSION"
echo "  commit    $(git -C "$ROOT" rev-parse --short HEAD)"

# The Tauri webview and the widget's layer-shell each need a dev package to
# link against. Missing, the build fails deep in a build script instead of here.
# (gtk4-layer-shell's pkg-config module is suffixed -0.)
libs_missing=
for lib in webkit2gtk-4.1 gtk4-layer-shell-0; do
    pkg-config --exists "$lib" 2>/dev/null || libs_missing="$libs_missing $lib"
done
if [ -n "$libs_missing" ]; then
    cat >&2 <<EOF
release.sh: missing build libraries:$libs_missing

  Arch / CachyOS:  sudo pacman -S --needed webkit2gtk-4.1 gtk4 gtk4-layer-shell \\
                     base-devel curl wget file openssl librsvg libappindicator-gtk3
  Debian / Ubuntu: sudo apt install libwebkit2gtk-4.1-dev libgtk-4-dev \\
                     libgtk4-layer-shell-dev libayatana-appindicator3-dev librsvg2-dev

  Full list: docs/02-development.md
EOF
    exit 1
fi

# --- 2. checks ---------------------------------------------------------

if [ "$run_tests" = yes ]; then
    say "checks (daemon: test + clippy)"
    (cd "$ROOT/daemon" && cargo test --workspace --locked &&
        cargo clippy --all-targets --locked -- -D warnings)

    say "checks (osd: test + clippy)"
    (cd "$ROOT/osd" && cargo test --locked &&
        cargo clippy --all-targets --locked -- -D warnings)

    say "checks (app: bun install + svelte-check)"
    (cd "$ROOT/app" && bun install --frozen-lockfile && bun run check)
else
    echo "  --skip-tests: not running the CI checks"
fi

# --- 3. build --------------------------------------------------------

say "build: daemon (release)"
(cd "$ROOT/daemon" && cargo build --release --locked \
    -p pyren-daemon -p pyren-ctl -p pyren-check)

say "build: widget (release)"
(cd "$ROOT/osd" && cargo build --release --locked)

# The app is built through `tauri build`, not a plain `cargo build`: only the
# CLI enables `tauri/custom-protocol`, and without it `generate_context!`
# embeds an *empty* asset set (dev mode + a configured devUrl), producing a
# binary that only works with `vite dev` running. `--no-bundle` keeps it to
# the one self-contained ELF - no dpkg/rpmbuild/linuxdeploy needed. It runs
# `beforeBuildCommand` (`bun run build`) itself.
(cd "$ROOT/app" && bun install --frozen-lockfile)

APPIMAGE=
if [ "$appimage" = yes ]; then
    say "build: app + AppImage (release)"
    echo "  note: Tauri fetches linuxdeploy on first use; the host needs FUSE"
    (cd "$ROOT/app" && bun run tauri build --ci --bundles appimage)
    APPIMAGE=$(find "$ROOT/app/src-tauri/target/release/bundle/appimage" \
        -maxdepth 1 -name '*.AppImage' -print 2>/dev/null | head -n1)
    [ -n "$APPIMAGE" ] || die "AppImage build reported success but no .AppImage was found"
else
    say "build: app (release, no bundle)"
    (cd "$ROOT/app" && bun run tauri build --ci --no-bundle)
fi

[ -x "$ROOT/app/src-tauri/target/release/pyren" ] ||
    die "the app binary is missing after the build"

# --- 4. stage ------------------------------------------------------

NAME="pyren-$VERSION"
DEST="$STAGE/$NAME"

say "stage: $DEST"
rm -rf "$STAGE"
mkdir -p "$DEST/bin" "$DEST/share/pyren" "$DEST/share/applications" \
    "$DEST/share/icons/hicolor/scalable/apps" "$DEST/lib/systemd/user"

install -m755 "$ROOT/daemon/target/release/pyren-daemon" "$DEST/bin/"
install -m755 "$ROOT/daemon/target/release/pyren-ctl" "$DEST/bin/"
install -m755 "$ROOT/daemon/target/release/pyren-check" "$DEST/bin/"
install -m755 "$ROOT/osd/target/release/pyren-osd" "$DEST/bin/"
install -m755 "$ROOT/app/src-tauri/target/release/pyren" "$DEST/bin/"

mkdir -p "$DEST/share/pyren/driver"
cp -R "$ROOT/driver/." "$DEST/share/pyren/driver/"

install -m644 "$ROOT/install/pyren.desktop" "$DEST/share/applications/pyren.desktop"
install -Dm644 "$ROOT/app/src-tauri/icons/32x32.png" "$DEST/share/icons/hicolor/32x32/apps/pyren.png"
install -Dm644 "$ROOT/app/src-tauri/icons/128x128.png" "$DEST/share/icons/hicolor/128x128/apps/pyren.png"
install -Dm644 "$ROOT/app/src-tauri/icons/128x128@2x.png" "$DEST/share/icons/hicolor/256x256/apps/pyren.png"
install -m644 "$ROOT/app/src-tauri/icons/icon.svg" "$DEST/share/icons/hicolor/scalable/apps/pyren.svg"

install -m644 "$ROOT/osd/pyren-osd.service" "$DEST/lib/systemd/user/pyren-osd.service"

install -m755 "$ROOT/install/install.sh" "$DEST/install.sh"
install -m755 "$ROOT/install/uninstall.sh" "$DEST/uninstall.sh"
install -m644 "$ROOT/install/INSTALL.md" "$DEST/INSTALL.md"
install -m644 "$ROOT/LICENSE" "$DEST/LICENSE"
install -m644 "$ROOT/NOTICE" "$DEST/NOTICE"

strip "$DEST"/bin/* 2>/dev/null || true

cat >"$DEST/README.md" <<EOF
# Pyren $VERSION

A clone of HP's OMEN Gaming Hub for Linux: a privileged Rust daemon plus an
unprivileged Tauri desktop app. <https://github.com/mglourido/PYREN>

## Install

\`\`\`sh
sha256sum -c SHA256SUMS           # optional: verify the download
sudo ./install.sh
newgrp pyren                      # or log out and back in
\`\`\`

That installs the daemon (systemd service), the app, the widget and the two
CLIs under \`/usr/local\`, the patched hp-wmi sources under
\`/usr/share/pyren\`, and adds you to the \`pyren\` group. Launch **Pyren**
from your app menu or run \`pyren\`; \`pyren-check\` reports what this laptop
can be told to do.

**\`INSTALL.md\` has the full guide** — requirements, every path touched,
\`--prefix\` / \`--dry-run\`, and \`./uninstall.sh\`.

Needs WebKitGTK (\`webkit2gtk-4.1\`) and \`gtk4\` + \`gtk4-layer-shell\`;
most desktops already have them.

## Licence

GPL-3.0-or-later. See \`LICENSE\` and \`NOTICE\`.
EOF

# --- 5. package -------------------------------------------------

say "package"
mkdir -p "$OUT"
TARBALL="$OUT/$NAME-x86_64-linux.tar.gz"
rm -f "$TARBALL" "$OUT/SHA256SUMS"
tar -C "$STAGE" --owner=0 --group=0 -czf "$TARBALL" "$NAME"

ARTIFACTS="$TARBALL"
if [ -n "$APPIMAGE" ]; then
    cp "$APPIMAGE" "$OUT/$NAME-x86_64.AppImage"
    chmod +x "$OUT/$NAME-x86_64.AppImage"
    ARTIFACTS="$ARTIFACTS $OUT/$NAME-x86_64.AppImage"
fi

# shellcheck disable=SC2086
(cd "$OUT" && sha256sum $(for f in $ARTIFACTS; do basename "$f"; done) >SHA256SUMS)
ARTIFACTS="$ARTIFACTS $OUT/SHA256SUMS"

echo
ls -lh "$OUT" | sed 's/^/  /'
echo
echo "  sha256:"
sed 's/^/    /' "$OUT/SHA256SUMS"

# --- 6. publish (draft only) ---------------------------------

if [ "$publish" = yes ]; then
    say "publish: draft GitHub release v$VERSION"
    TAG="v$VERSION"
    notes=$(mktemp)
    # The CHANGELOG section for this version, from its heading to the next.
    awk -v v="$VERSION" '
        $0 ~ "^## \\[" v "\\]" {f=1; next}
        f && /^## \[/ {exit}
        f {print}
    ' "$ROOT/CHANGELOG.md" 2>/dev/null | sed '/./,$!d' >"$notes" || true
    if ! grep -q . "$notes"; then
        printf 'Pyren %s\n\n_No CHANGELOG.md section for %s - edit before publishing._\n' \
            "$TAG" "$VERSION" >"$notes"
    fi

    # shellcheck disable=SC2086
    gh release create "$TAG" $ARTIFACTS \
        --draft \
        --title "Pyren $TAG" \
        --notes-file "$notes"
    rm -f "$notes"
    echo
    echo "  Draft created. Review it, then publish from the GitHub Releases page."
else
    say "next"
    cat <<EOF
  Verify the archive on a clean machine (or container):
      tar xzf $OUT/$NAME-x86_64-linux.tar.gz
      cd $NAME && sudo ./install.sh && pyren-check

  Then cut the release (see install/INSTALL.md):
      git tag -a v$VERSION -m "Pyren v$VERSION"
      git push --follow-tags
      tools/release.sh --skip-tests --publish     # or upload dist/* by hand
EOF
fi
