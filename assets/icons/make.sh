#!/usr/bin/env bash
#
# Rasterises the menu's glyphs from Lucide (ISC, https://lucide.dev) into
# glyph/<name>.png next to this script, which src/art.rs embeds in the binary.
# Black on clear: macOS tints them for light and dark menus, Windows draws them
# in the menu's text colour.
#
# Needs npm (to fetch the pinned package) and rsvg-convert (brew install
# librsvg). The output is committed, so building Sonus needs neither.
#
#   assets/icons/make.sh
set -euo pipefail
cd "$(dirname "$0")"

LUCIDE=0.545.0   # same pin as Café and Porta, so the apps' menus match
EDGE=32          # menus draw icons at 16 pt: 32 px is sharp on Retina and at 200%
PAD=2            # breathing room, so glyphs sit at the size of the OS's own icons

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
(cd "$work" && npm pack --silent "lucide-static@$LUCIDE" >/dev/null)
mkdir -p "$work/lucide"
tar xzf "$work/lucide-static-$LUCIDE.tgz" -C "$work/lucide"

raster() { # svg png
  local inner=$((EDGE - 2 * PAD))
  rsvg-convert -w "$inner" -h "$inner" --keep-aspect-ratio \
    --page-width "$EDGE" --page-height "$EDGE" --left "$PAD" --top "$PAD" \
    -o "$2" "$1"
}

# Sonus name = Lucide name.
GLYPHS=(
  settings file-cog scroll-text power
)
rm -rf glyph
mkdir -p glyph
for name in "${GLYPHS[@]}"; do
  raster "$work/lucide/package/icons/$name.svg" "glyph/$name.png"
done

{
  echo "# Icon license"
  echo
  echo "The PNGs in \`glyph/\` are rasterised by \`make.sh\` from Lucide $LUCIDE."
  echo
  echo "## glyph/: Lucide $LUCIDE"
  echo
  echo "https://github.com/lucide-icons/lucide"
  echo
  cat "$work/lucide/package/LICENSE"
} > LICENSES.md

echo "$(ls glyph | wc -l | tr -d ' ') glyphs"
