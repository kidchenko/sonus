#!/usr/bin/env bash
#
# Builds Sonus.app for macOS. The counterpart to build.ps1.
#
#   ./build.sh                # test + release binary + Sonus.app (this Mac's arch)
#   ./build.sh --universal    # ... as one arm64 + x86_64 binary
#   ./build.sh --dmg          # ... then wrap Sonus.app in a .dmg
#   ./build.sh --run          # ... then launch it in the menu bar
#   ./build.sh --skip-tests
set -euo pipefail
cd "$(dirname "$0")"

run=0; dmg=0; universal=0; skip_tests=0
for arg in "$@"; do
  case "$arg" in
    --run) run=1 ;;
    --dmg) dmg=1 ;;
    --universal) universal=1 ;;
    --skip-tests) skip_tests=1 ;;
    *) echo "unknown option: $arg" >&2; exit 1 ;;
  esac
done

cyan() { printf '\n\033[36m==> %s\033[0m\n' "$1"; }
ok()   { printf '    \033[32m%s\033[0m\n' "$1"; }
warn() { printf '    \033[33m%s\033[0m\n' "$1"; }

cargo="$(command -v cargo || true)"
[ -z "$cargo" ] && [ -x "$HOME/.cargo/bin/cargo" ] && cargo="$HOME/.cargo/bin/cargo"
if [ -z "$cargo" ]; then
  echo "cargo not found: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
  exit 1
fi

# [package].version, not the first `version =` in the file.
version="$(awk '/^\[/{p=($0=="[package]")} p&&/^version[[:space:]]*=/{split($0,a,"\"");print a[2];exit}' Cargo.toml)"
[ -n "$version" ] || { echo 'could not read [package].version from Cargo.toml' >&2; exit 1; }

if [ "$skip_tests" -eq 0 ]; then
  cyan 'Running tests'
  "$cargo" test --quiet
  ok 'tests passed'
fi

cyan 'Building release binary'
if [ "$universal" -eq 1 ]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  "$cargo" build --release --target aarch64-apple-darwin
  "$cargo" build --release --target x86_64-apple-darwin
  mkdir -p target/universal
  exe=target/universal/sonus
  lipo -create -output "$exe" \
    target/aarch64-apple-darwin/release/sonus target/x86_64-apple-darwin/release/sonus
  ok "universal: $(lipo -archs "$exe")"
else
  "$cargo" build --release
  exe=target/release/sonus
fi
ok "$(printf 'sonus   %.2f MB' "$(echo "scale=2; $(stat -f%z "$exe")/1048576" | bc)")"

cyan 'Assembling Sonus.app'
app=dist/Sonus.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$exe" "$app/Contents/MacOS/sonus"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
printf 'APPL????' > "$app/Contents/PkgInfo"

# A full iconset from the 1024 master, so Finder and the DMG get crisp art at
# every size rather than one upscaled bitmap.
if command -v iconutil >/dev/null && [ -f assets/icon-1024.png ]; then
  set_dir="$(mktemp -d)/Sonus.iconset"
  mkdir -p "$set_dir"
  for s in 16 32 128 256 512; do
    sips -z $s $s assets/icon-1024.png --out "$set_dir/icon_${s}x${s}.png" >/dev/null
    sips -z $((s*2)) $((s*2)) assets/icon-1024.png --out "$set_dir/icon_${s}x${s}@2x.png" >/dev/null
  done
  iconutil -c icns "$set_dir" -o "$app/Contents/Resources/Sonus.icns" && ok 'Sonus.icns'
else
  warn 'iconutil or assets/icon-1024.png missing; the app will use a default icon'
fi

# Ad-hoc signature: a stable identity for Gatekeeper on this machine. Real
# distribution wants a Developer ID + notarization (see README).
codesign --force --sign - "$app" >/dev/null 2>&1 && ok 'ad-hoc signed' || warn 'codesign failed; unsigned'
ok "$app ($version)"

if [ "$dmg" -eq 1 ]; then
  cyan 'Building disk image'
  stage="$(mktemp -d)"
  cp -R "$app" "$stage/"
  # The drag-to-install target.
  ln -s /Applications "$stage/Applications"
  dmg_path="dist/Sonus-$version.dmg"
  rm -f "$dmg_path"
  hdiutil create -quiet -volname Sonus -srcfolder "$stage" -ov -format UDZO "$dmg_path"
  # Same name every release, for /releases/latest/download/Sonus.dmg.
  cp -f "$dmg_path" dist/Sonus.dmg
  ok "$dmg_path"
  ok "sha256  $(shasum -a 256 "$dmg_path" | cut -d' ' -f1)"
fi

if [ "$run" -eq 1 ]; then
  cyan 'Launching'
  pkill -x sonus 2>/dev/null || true
  open "$app"
  ok 'running in the menu bar'
fi
