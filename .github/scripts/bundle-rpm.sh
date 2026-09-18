#!/bin/bash
# Usage: bundle-rpm.sh <target-triple> <arch-label>
# Package the release binary into an RPM:
#   dist/nermal-<version>-linux-<arch>.rpm
#
# Same payload as the tarball (bundle-linux.sh) — the binary, the CLI, the
# completion specs — plus the desktop entry and icon AppImage already
# generates, so a Fedora/openSUSE/RHEL user gets a Start Menu entry via
# `dnf install` instead of unpacking a tarball by hand. rpmbuild builds
# straight from a pre-populated buildroot (empty %build/%install) rather than
# compiling in the spec, since the release binary already exists — the same
# "stage files, then archive" shape bundle-linux.sh and bundle-appimage.sh use.
#
# Updates go through the distro's own package manager here, not the in-app
# updater: unlike the AppImage and the Windows installer, an RPM install lands
# under root-owned system paths the updater has no business rewriting, and
# `dnf upgrade` already exists for this. So nermal-updater is not bundled,
# matching the plain tarball.
set -euo pipefail

TARGET="$1"
ARCH="$2"
# Anchored on `= "` — see the note in bundle-macos.sh: the root manifest leads
# with `version.workspace = true`, which a bare `^version` match would return
# verbatim as the "version" and bake into every asset filename.
VERSION="$(grep -m1 '^version = "' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]]; then
  echo "bundle-rpm: could not read a version from Cargo.toml (got '$VERSION')" >&2
  exit 1
fi
NAME="nermal-${VERSION}-linux-${ARCH}"

# RPM's own arch names, not Rust's.
case "$ARCH" in
  x86_64) RPM_ARCH=x86_64 ;;
  arm64 | aarch64) RPM_ARCH=aarch64 ;;
  *) echo "unsupported arch for RPM: $ARCH" >&2; exit 1 ;;
esac

TOPDIR="$(mktemp -d)"
BUILDROOT="$TOPDIR/buildroot"
mkdir -p "$BUILDROOT/usr/bin" "$BUILDROOT/usr/share/applications" \
  "$BUILDROOT/usr/share/pixmaps" "$BUILDROOT/usr/share/licenses/nermal" \
  "$BUILDROOT/usr/share/doc/nermal"

cp "target/${TARGET}/release/nermal-app" "$BUILDROOT/usr/bin/nermal-app"
# The CLI ships beside the GUI, which symlinks it onto PATH at launch (see
# core::cli_install) by resolving it relative to its own executable.
cp "target/${TARGET}/release/nermal" "$BUILDROOT/usr/bin/nermal"
chmod 755 "$BUILDROOT/usr/bin/nermal-app" "$BUILDROOT/usr/bin/nermal"
strip "$BUILDROOT/usr/bin/nermal-app" || echo "⚠️  strip unavailable — shipping unstripped binary"
strip "$BUILDROOT/usr/bin/nermal" || echo "⚠️  strip unavailable — shipping unstripped CLI"

# Completion specs are loaded relative to the executable at runtime (see
# terminal::signature::spec_source), the same `<exe-dir>/completions` layout
# the AppImage uses — not the FHS `/usr/share` location a hand-rolled RPM
# would otherwise reach for.
mkdir -p "$BUILDROOT/usr/bin/completions"
cp assets/completions/*.json "$BUILDROOT/usr/bin/completions/"

cp LICENSE "$BUILDROOT/usr/share/licenses/nermal/LICENSE"
cp README.md "$BUILDROOT/usr/share/doc/nermal/README.md"

cat > "$BUILDROOT/usr/share/applications/nermal.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Nermal
Comment=A fast, native terminal
Exec=nermal-app
Icon=nermal
Categories=System;TerminalEmulator;
Terminal=false
StartupWMClass=nermal
DESKTOP

# rpmbuild wants a fixed icon file, same as linuxdeploy in bundle-appimage.sh.
convert assets/app-icon.png -resize 256x256 "$BUILDROOT/usr/share/pixmaps/nermal.png"

SPEC="$TOPDIR/nermal.spec"
cat > "$SPEC" <<SPECEOF
Name: nermal
Version: ${VERSION}
Release: 1
Summary: A fast, native terminal
License: MIT
URL: https://github.com/nermalcat69/nermal-ide
BuildArch: ${RPM_ARCH}
# Nothing to compile — the payload is already in place under %{buildroot}
# from the steps above, and rpmbuild is only asked to archive it. No sources
# to extract a debuginfo subpackage from either (the binary is stripped, and
# there is no %%install to run brp-strip against), so that stage is skipped
# outright rather than left to fail on a build-id-less binary.
%define _build_id_links none
%global debug_package %{nil}

%description
Nermal is a fast, native terminal.

%files
/usr/bin/nermal-app
/usr/bin/nermal
/usr/bin/completions/*.json
/usr/share/applications/nermal.desktop
/usr/share/pixmaps/nermal.png
/usr/share/licenses/nermal/LICENSE
/usr/share/doc/nermal/README.md
SPECEOF

rpmbuild -bb \
  --define "_topdir $TOPDIR" \
  --buildroot "$BUILDROOT" \
  --target "${RPM_ARCH}-linux" \
  "$SPEC"

mkdir -p dist
BUILT_RPM="$(find "$TOPDIR/RPMS" -name '*.rpm' -print -quit)"
if [[ -z "$BUILT_RPM" ]]; then
  echo "bundle-rpm: rpmbuild reported success but produced no .rpm" >&2
  exit 1
fi
mv "$BUILT_RPM" "dist/${NAME}.rpm"
rm -rf "$TOPDIR"
echo "✅ dist/${NAME}.rpm"
