#!/usr/bin/env bash
#
# Build a .deb for the host architecture (§55).
#
#   ./scripts/build-deb.sh            release build, then package
#   ./scripts/build-deb.sh --no-build use whatever is already in target/release
#
# Produces packaging/build/clipboard-manager_<version>_<arch>.deb, installable
# with:
#
#   sudo apt install ./clipboard-manager_<version>_<arch>.deb
#
# `apt install ./file.deb` rather than `dpkg -i` on purpose: apt resolves the
# dependencies, dpkg leaves them for you to chase.
#
# This is a hand-rolled dpkg-deb build rather than debhelper. The reason is the
# support window: dh-cargo on Ubuntu 20.04 predates the Rust edition and
# lockfile format this project uses, so a debian/rules build would need a
# different incantation per release. dpkg-deb consumes a plain directory tree
# and behaves identically on every target from Debian 11 to Ubuntu 26.04.

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
BUILD_DIR="$ROOT/packaging/build"

BUILD=1
if [[ "${1:-}" == "--no-build" ]]; then
    BUILD=0
fi

# ---- identity ---------------------------------------------------------------

VERSION="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml | head -1)"
if [[ -z "$VERSION" ]]; then
    echo "error: could not read the version from Cargo.toml" >&2
    exit 1
fi
ARCH="$(dpkg --print-architecture)"
PKG="clipboard-manager_${VERSION}_${ARCH}"
STAGE="$BUILD_DIR/$PKG"

echo "Building clipboard-manager $VERSION for $ARCH"

# ---- compile ----------------------------------------------------------------

if [[ "$BUILD" == 1 ]]; then
    # Capped concurrency: linking this dependency tree at full parallelism can
    # drive enough memory pressure for systemd-oomd to kill the user session.
    # See .cargo/config.toml.
    cargo build --release --jobs "${CARGO_JOBS:-2}"
fi

BINARY="$ROOT/target/release/clipboard-manager"
if [[ ! -x "$BINARY" ]]; then
    echo "error: $BINARY not found; run without --no-build" >&2
    exit 1
fi

# Refuse to package a binary older than the sources.
#
# This is not paranoia: `--no-build` once packaged a months-stale binary from
# before `main.rs` was written, producing a .deb whose `--version` printed
# nothing and whose dependency list was missing the entire GTK stack — because
# an empty `main` links none of it. It installed cleanly and did nothing.
NEWER="$(find "$ROOT/src" "$ROOT/Cargo.toml" "$ROOT/Cargo.lock" \
    -type f -newer "$BINARY" -print -quit 2>/dev/null || true)"
if [[ -n "$NEWER" ]]; then
    echo "error: target/release/clipboard-manager is older than the sources." >&2
    echo "       $NEWER has changed since it was built." >&2
    echo "       Run without --no-build, or rebuild with: cargo build --release" >&2
    exit 1
fi

# Sanity-check that it is a working binary and not a stub.
ACTUAL_VERSION="$("$BINARY" --version 2>/dev/null | awk '{print $NF}')"
if [[ "$ACTUAL_VERSION" != "$VERSION" ]]; then
    echo "error: $BINARY reports version '${ACTUAL_VERSION:-<nothing>}'," >&2
    echo "       but Cargo.toml says '$VERSION'. Refusing to package it." >&2
    exit 1
fi

# ---- stage the tree ---------------------------------------------------------

rm -rf "$STAGE"
install -d "$STAGE/DEBIAN"
install -d "$STAGE/usr/bin"
install -d "$STAGE/usr/share/applications"
install -d "$STAGE/usr/share/icons/hicolor/scalable/apps"
install -d "$STAGE/usr/share/man/man1"
install -d "$STAGE/usr/share/doc/clipboard-manager"

install -m 0755 "$BINARY" "$STAGE/usr/bin/clipboard-manager"
install -m 0644 packaging/clipboard-manager.desktop \
    "$STAGE/usr/share/applications/clipboard-manager.desktop"
install -m 0644 assets/icons/clipboard-manager.svg \
    "$STAGE/usr/share/icons/hicolor/scalable/apps/clipboard-manager.svg"

gzip -9n < packaging/clipboard-manager.1 \
    > "$STAGE/usr/share/man/man1/clipboard-manager.1.gz"
chmod 0644 "$STAGE/usr/share/man/man1/clipboard-manager.1.gz"

install -m 0644 README.md "$STAGE/usr/share/doc/clipboard-manager/README.md"
install -m 0644 LICENSE "$STAGE/usr/share/doc/clipboard-manager/copyright"

# Debian wants a changelog, gzipped, at a fixed path.
{
    echo "clipboard-manager ($VERSION) unstable; urgency=low"
    echo
    echo "  * See https://github.com/sridharreddy23/clipboard-manager/releases"
    echo
    echo " -- sridhar <sridharsri789tej@gmail.com>  $(date -R)"
} | gzip -9n > "$STAGE/usr/share/doc/clipboard-manager/changelog.Debian.gz"
chmod 0644 "$STAGE/usr/share/doc/clipboard-manager/changelog.Debian.gz"

# ---- dependencies -----------------------------------------------------------

# Derived from the binary rather than hardcoded, so the package declares what it
# actually links against on the distribution it was built on. A hardcoded list
# is how a .deb ends up uninstallable on one of the seven targets.
echo "Resolving shared library dependencies..."
DEPENDS="$(
    if command -v dpkg-shlibdeps >/dev/null 2>&1; then
        pushd "$STAGE" >/dev/null
        install -d debian
        echo 9 > debian/compat
        # dpkg-shlibdeps insists on a debian/control existing.
        printf 'Source: clipboard-manager\n\nPackage: clipboard-manager\nArchitecture: any\n' \
            > debian/control
        dpkg-shlibdeps -O --ignore-missing-info usr/bin/clipboard-manager 2>/dev/null \
            | sed 's/^shlibs:Depends=//'
        rm -rf debian
        popd >/dev/null
    fi
)"

if [[ -z "$DEPENDS" ]]; then
    # Fallback: the GTK3 stack, present on every supported release. Named
    # without versions so one list works across the whole window.
    echo "  dpkg-shlibdeps unavailable; using the static GTK3 dependency list"
    DEPENDS="libc6, libgtk-3-0 | libgtk-3-0t64, libglib2.0-0 | libglib2.0-0t64, libcairo2, libpango-1.0-0, libgdk-pixbuf-2.0-0, libx11-6"
fi
echo "  Depends: $DEPENDS"

SIZE="$(du -ks "$STAGE" | cut -f1)"

sed -e "s|@VERSION@|$VERSION|g" \
    -e "s|@ARCH@|$ARCH|g" \
    -e "s|@DEPENDS@|$DEPENDS|g" \
    -e "s|@SIZE@|$SIZE|g" \
    packaging/debian/control > "$STAGE/DEBIAN/control"

install -m 0755 packaging/debian/postinst "$STAGE/DEBIAN/postinst"
install -m 0755 packaging/debian/prerm "$STAGE/DEBIAN/prerm"

# Config files the package does not own; listing none is correct here because
# everything user-specific is created at runtime under $HOME, not by the
# package.
: > "$STAGE/DEBIAN/conffiles"
rm -f "$STAGE/DEBIAN/conffiles"

# md5sums, which dpkg uses to detect local modifications.
pushd "$STAGE" >/dev/null
find usr -type f -print0 | sort -z | xargs -0 md5sum > DEBIAN/md5sums
chmod 0644 DEBIAN/md5sums
popd >/dev/null

# ---- build and verify -------------------------------------------------------

DEB="$BUILD_DIR/$PKG.deb"
fakeroot dpkg-deb --build --root-owner-group "$STAGE" "$DEB" >/dev/null
rm -rf "$STAGE"

echo
echo "Built $DEB"
dpkg-deb --info "$DEB" | sed 's/^/  /'
echo "Contents:"
dpkg-deb --contents "$DEB" | awk '{print "  " $6, $7, $8}'

if command -v lintian >/dev/null 2>&1; then
    echo
    echo "lintian:"
    # Informational: a hand-rolled package trips several Debian-policy tags
    # that do not matter for a directly distributed .deb.
    lintian --no-tag-display-limit "$DEB" 2>&1 | sed 's/^/  /' || true
fi

echo
echo "Install with:"
echo "  sudo apt install $DEB"
