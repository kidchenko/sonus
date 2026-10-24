#!/usr/bin/env bash
#
# Builds Sonus for Linux: the counterpart to build.sh (macOS) and build.ps1,
# and what build.sh runs when it finds itself on Linux.
#
#   ./build.sh                # test + release binary
#   ./build.sh --package      # ... then dist/: a .deb, an .rpm and a .tar.gz
#   ./build.sh --skip-tests
#
# The packages use the system's GTK and sound libraries rather than carrying
# their own: every desktop has them, and they are what makes Sonus look and
# sound at home. Their dependencies are read off the binary, plus the tray
# library, which is opened at run time and so is not on that list.
#
# Packaging needs dpkg-deb and dpkg-shlibdeps (dpkg-dev) for the .deb and
# rpmbuild (rpm) and file for the .rpm; each is skipped, with a warning, if
# missing.
set -euo pipefail
cd "$(dirname "$0")/../.."

package=0; skip_tests=0
for arg in "$@"; do
  case "$arg" in
    --package) package=1 ;;
    --skip-tests) skip_tests=1 ;;
    *) echo "unknown option: $arg" >&2; exit 1 ;;
  esac
done

cyan() { printf '\n\033[36m==> %s\033[0m\n' "$1"; }
ok()   { printf '    \033[32m%s\033[0m\n' "$1"; }
warn() { printf '    \033[33m%s\033[0m\n' "$1"; }

version="$(awk '/^\[/{p=($0=="[package]")} p&&/^version[[:space:]]*=/{split($0,a,"\"");print a[2];exit}' Cargo.toml)"
[ -n "$version" ] || { echo 'could not read [package].version from Cargo.toml' >&2; exit 1; }
target_dir="${CARGO_TARGET_DIR:-target}"

if [ "$skip_tests" -eq 0 ]; then
  cyan 'Running tests'
  cargo test --quiet
  ok 'tests passed'
fi

cyan 'Building release binary'
cargo build --release
exe="$(realpath "$target_dir/release/sonus")"
ok "$(printf 'sonus   %.2f MB' "$(echo "scale=2; $(stat -c%s "$exe")/1048576" | bc)")"

[ "$package" -eq 1 ] || exit 0

arch="$(uname -m)"
case "$arch" in
  x86_64) deb_arch=amd64 ;;
  aarch64) deb_arch=arm64 ;;
  *) deb_arch="$arch" ;;
esac

# The installed tree, as it lands under /usr.
cyan 'Staging'
root="$(mktemp -d)"
install -Dm755 "$exe" "$root/usr/bin/sonus"
install -Dm644 packaging/linux/sonus.desktop "$root/usr/share/applications/sonus.desktop"
install -Dm644 LICENSE "$root/usr/share/doc/sonus/copyright"
icons="$(mktemp -d)"
"$exe" --dump-icons "$icons" >/dev/null
python3 - "$icons" "$root/usr/share/icons/hicolor" <<'PY'
import pathlib, sys
sys.path.insert(0, "assets")
from make_icons import png
src, dst = map(pathlib.Path, sys.argv[1:])
for edge in (16, 24, 32, 48, 64, 128, 256):
    out = dst / f"{edge}x{edge}" / "apps" / "sonus.png"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_bytes(png((src / f"app-{edge}.rgba").read_bytes(), edge))
PY
ok "$(find "$root" -type f | wc -l) files"
mkdir -p dist

cyan 'Building .tar.gz'
tarball="dist/sonus-$version-$arch-linux.tar.gz"
tar -C "$root/usr" -czf "$tarball" --owner=0 --group=0 bin share
# Same name every release, for /releases/latest/download/, as Sonus.dmg.
cp -f "$tarball" "dist/sonus-$arch-linux.tar.gz"
ok "$tarball (unpack into /usr/local or ~/.local)"

cyan 'Building .deb'
if command -v dpkg-deb >/dev/null && command -v dpkg-shlibdeps >/dev/null; then
  work="$(mktemp -d)"
  mkdir -p "$work/debian"
  echo 'Source: sonus' > "$work/debian/control"
  # Its one warning is that the binary is not yet in a package's tree.
  shlibs="$(cd "$work" && dpkg-shlibdeps -O "$exe" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')"
  [ -n "$shlibs" ] || { echo 'dpkg-shlibdeps found no dependencies' >&2; exit 1; }
  mkdir -p "$root/DEBIAN"
  cat > "$root/DEBIAN/control" <<CONTROL
Package: sonus
Version: $version
Architecture: $deb_arch
Maintainer: kidchenko <kidchenko@gmail.com>
Depends: $shlibs, libayatana-appindicator3-1 | libappindicator3-1
Section: sound
Priority: optional
Homepage: https://josebarbosa.dev/sonus/
Description: A volume slider for every app, in the tray
 Sonus puts a slider, a level meter and a mute button for every app playing
 sound in your tray, with the output device at the top, and play, pause and
 skip for players that offer them.
CONTROL
  deb="dist/sonus_${version}_$deb_arch.deb"
  dpkg-deb --root-owner-group --build "$root" "$deb" >/dev/null
  cp -f "$deb" "dist/sonus_$deb_arch.deb"
  rm -rf "$root/DEBIAN"
  ok "$deb"
  ok "depends: $shlibs"
else
  warn 'dpkg-deb or dpkg-shlibdeps missing (apt install dpkg-dev); skipping the .deb'
fi

cyan 'Building .rpm'
if command -v rpmbuild >/dev/null && command -v file >/dev/null; then
  top="$(mktemp -d)"
  mkdir -p "$top/SPECS"
  # The libraries the binary links are found by rpm itself; the tray library
  # is asked for by its soname, which Fedora and openSUSE both provide.
  cat > "$top/SPECS/sonus.spec" <<SPEC
Name:      sonus
Version:   $version
Release:   1
Summary:   A volume slider for every app, in the tray
License:   MIT
URL:       https://josebarbosa.dev/sonus/
Requires:  libayatana-appindicator3.so.1()(64bit)

%description
Sonus puts a slider, a level meter and a mute button for every app playing
sound in your tray, with the output device at the top, and play, pause and
skip for players that offer them.

%install
cp -a $root/. %{buildroot}/

%files
/usr/bin/sonus
/usr/share/applications/sonus.desktop
/usr/share/icons/hicolor/*/apps/sonus.png
%license /usr/share/doc/sonus/copyright
SPEC
  rpmbuild --quiet -bb --define "_topdir $top" --define "_build_id_links none" \
    "$top/SPECS/sonus.spec" >/dev/null
  rpm_file="$(ls "$top"/RPMS/*/*.rpm)"
  cp "$rpm_file" dist/
  cp -f "$rpm_file" "dist/sonus.$arch.rpm"
  ok "dist/$(basename "$rpm_file")"
else
  warn 'rpmbuild or file missing (apt install rpm file); skipping the .rpm'
fi
