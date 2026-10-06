#!/usr/bin/env bash
# Build release packages into out/.
#
#   scripts/package.sh            # .deb and AppImage
#   scripts/package.sh deb
#   scripts/package.sh appimage
set -euo pipefail

cd "$(dirname "$0")/.."
target="${1:-all}"
case "$target" in
    all | deb | appimage) ;;
    *) echo "usage: $0 [all|deb|appimage]" >&2; exit 2 ;;
esac

version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)"
deb_arch="$(dpkg --print-architecture)"
arch="$(uname -m)"
out="out"
build="$out/build"

# The build packs the app registrations in from the environment or .env.
# Say so when one is missing: such a package would ask users for a client ID.
for name in SKYDOCK_ONEDRIVE_CLIENT_ID SKYDOCK_GDRIVE_CLIENT_ID SKYDOCK_GDRIVE_CLIENT_SECRET; do
    if [ -z "${!name:-}" ] && ! grep -qE "^(export )?$name=.+" .env 2>/dev/null; then
        echo "warning: $name is not set in the environment or .env; it will not be packed in" >&2
    fi
done

echo "==> Building Skydock $version (release)"
cargo build --release -p skydock-cli -p skydock-gui
rm -rf "$build"
mkdir -p "$build"

# Oldest glibc the packages must run on: 2.39 is Ubuntu 24.04. A binary that
# needs anything newer would install but fail to start there.
max_glibc="2.39"
for binary in target/release/skydock target/release/skydock-gui; do
    needed="$(objdump -T "$binary" | grep -oE 'GLIBC_[0-9.]+' | sort -uV | tail -1)"
    needed="${needed#GLIBC_}"
    if [ "$(printf '%s\n%s\n' "$needed" "$max_glibc" | sort -V | tail -1)" != "$max_glibc" ]; then
        echo "error: $binary needs glibc $needed, newer than the supported $max_glibc:" >&2
        objdump -T "$binary" | grep "GLIBC_$needed" >&2
        exit 1
    fi
done
echo "    binaries need glibc <= $max_glibc"

# Files common to both package formats, laid out under a /usr prefix.
install_tree() {
    local root="$1"
    install -Dm755 target/release/skydock "$root/usr/bin/skydock"
    install -Dm755 target/release/skydock-gui "$root/usr/bin/skydock-gui"
    install -Dm644 packaging/skydock.desktop "$root/usr/share/applications/skydock.desktop"
    install -Dm644 packaging/skydock.svg "$root/usr/share/icons/hicolor/scalable/apps/skydock.svg"
    install -Dm644 packaging/skydock-symbolic.svg "$root/usr/share/icons/hicolor/symbolic/apps/skydock-symbolic.svg"
    install -Dm644 LICENSE "$root/usr/share/doc/skydock/copyright"
}

build_deb() {
    echo "==> Building .deb"
    local root="$build/deb"
    install_tree "$root"

    # Let dpkg work out the shared-library dependencies from the binaries.
    # dpkg-shlibdeps insists on a debian/control file in the working directory.
    local work="$build/shlibdeps"
    mkdir -p "$work/debian"
    printf 'Source: skydock\n\nPackage: skydock\nArchitecture: any\n' > "$work/debian/control"
    local depends
    depends="$(cd "$work" && dpkg-shlibdeps -O \
        "$OLDPWD/$root/usr/bin/skydock" "$OLDPWD/$root/usr/bin/skydock-gui" 2>/dev/null |
        sed -n 's/^shlibs:Depends=//p')"

    mkdir -p "$root/DEBIAN"
    cat > "$root/DEBIAN/control" <<CONTROL
Package: skydock
Version: $version
Section: net
Priority: optional
Architecture: $deb_arch
Depends: $depends, fuse3
Recommends: libxkbcommon0, libwayland-client0, libegl1, xdg-utils
Installed-Size: $(du -sk "$root/usr" | cut -f1)
Maintainer: $(git config user.name) <$(git config user.email)>
Description: Cloud drive client for OneDrive and Google Drive
 Skydock signs in to cloud drives through the browser and keeps a local
 view of their contents, with a desktop window, tray icon and command-line
 tool.
CONTROL
    dpkg-deb --build --root-owner-group "$root" "$out/skydock_${version}_${deb_arch}.deb" >/dev/null
    echo "    $out/skydock_${version}_${deb_arch}.deb"
}

build_appimage() {
    echo "==> Building AppImage"
    local appdir="$build/Skydock.AppDir"
    install_tree "$appdir"
    cp packaging/skydock.desktop packaging/skydock.svg "$appdir/"

    # Carry the libraries a base desktop system is not guaranteed to have.
    # Everything else (glibc, fontconfig, graphics drivers) must come from
    # the host, as the AppImage convention requires.
    mkdir -p "$appdir/usr/lib"
    ldd "$appdir/usr/bin/skydock-gui" "$appdir/usr/bin/skydock" |
        awk '/libsqlite3/ { print $3 }' | sort -u |
        xargs -r -I{} cp -L {} "$appdir/usr/lib/"

    cat > "$appdir/AppRun" <<'APPRUN'
#!/bin/sh
here="$(dirname "$(readlink -f "$0")")"
export LD_LIBRARY_PATH="$here/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
# `Skydock.AppImage cli ...` runs the command-line tool instead of the window.
if [ "${1:-}" = "cli" ]; then
    shift
    exec "$here/usr/bin/skydock" "$@"
fi
exec "$here/usr/bin/skydock-gui" "$@"
APPRUN
    chmod +x "$appdir/AppRun"

    local tool
    if command -v appimagetool >/dev/null; then
        tool="appimagetool"
    else
        tool="$out/tools/appimagetool-$arch.AppImage"
        if [ ! -x "$tool" ]; then
            echo "    downloading appimagetool"
            mkdir -p "$out/tools"
            curl -fL --progress-bar -o "$tool" \
                "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$arch.AppImage"
            chmod +x "$tool"
        fi
    fi
    # Extract-and-run so the tool itself does not need FUSE 2.
    # Build beside the target and rename over it: the previous AppImage may
    # be running, and a running file cannot be written to but can be replaced.
    local image="$out/Skydock-$version-$arch.AppImage"
    ARCH="$arch" "$tool" --appimage-extract-and-run "$appdir" "$build/Skydock.AppImage" \
        >"$build/appimagetool.log" 2>&1 ||
        { cat "$build/appimagetool.log" >&2; exit 1; }
    mv -f "$build/Skydock.AppImage" "$image"
    echo "    $out/Skydock-$version-$arch.AppImage"
}

[ "$target" = appimage ] || build_deb
[ "$target" = deb ] || build_appimage
