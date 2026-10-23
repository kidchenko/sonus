"""Turn `sonus --dump-icons <dir>` output into the committed icon assets.

    sonus --dump-icons target/icons
    python assets/make_icons.py target/icons

Writes, next to this script:
  icon.ico       every size up to 256, PNG-compressed entries (Vista+)
  icon-256.png   for the Chocolatey/winget listings and the README
  icon-1024.png  the master build.sh turns into the macOS .icns

and the landing page's favicons into docs/img: favicon.ico (16, 32, 48) and
favicon-16.png / favicon-32.png. Those come from the app's own small sizes,
which are drawn for small sizes (a single fader at 16), rather than letting
the browser shrink the 256 px icon to a smudge.

It also leaves a PNG of every tray face and glyph in the dump directory, for
looking at.

Standard library only: the app draws its own pixels, so the only job here is
container formats.
"""

import pathlib
import struct
import sys
import zlib

HERE = pathlib.Path(__file__).resolve().parent
ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]


def png_rect(rgba: bytes, w: int, h: int) -> bytes:
    raw = b"".join(b"\x00" + rgba[y * w * 4:(y + 1) * w * 4] for y in range(h))

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def png(rgba: bytes, edge: int) -> bytes:
    return png_rect(rgba, edge, edge)


def ico(images: list[tuple[int, bytes]]) -> bytes:
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries, blobs = b"", b""
    for edge, data in images:
        # 0 in the width/height byte means 256.
        dim = 0 if edge >= 256 else edge
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        blobs += data
        offset += len(data)
    return header + entries + blobs


def main(dump: pathlib.Path) -> None:
    def load(name: str, edge: int) -> bytes:
        data = (dump / f"{name}-{edge}.rgba").read_bytes()
        assert len(data) == edge * edge * 4, f"{name}-{edge}: wrong size"
        return data

    # Through a temp file: Explorer holds icon.ico open while it shows the
    # folder, and a replace succeeds where opening it for writing does not.
    tmp = HERE / "icon.ico.tmp"
    tmp.write_bytes(ico([(e, png(load("app", e), e)) for e in ICO_SIZES]))
    tmp.replace(HERE / "icon.ico")
    for edge in (256, 1024):
        (HERE / f"icon-{edge}.png").write_bytes(png(load("app", edge), edge))

    site = HERE.parent / "docs" / "img"
    site.mkdir(parents=True, exist_ok=True)
    (site / "favicon.ico").write_bytes(ico([(e, png(load("app", e), e)) for e in (16, 32, 48)]))
    for edge in (16, 32):
        (site / f"favicon-{edge}.png").write_bytes(png(load("app", edge), edge))

    for pattern in ("tray-*.rgba", "glyph-*.rgba"):
        for path in sorted(dump.glob(pattern)):
            edge = int(path.stem.rsplit("-", 1)[1])
            path.with_suffix(".png").write_bytes(png(path.read_bytes(), edge))

    print(f"wrote icon.ico, icon-256.png, icon-1024.png to {HERE}")
    print(f"wrote favicon.ico, favicon-16.png, favicon-32.png to {site}")
    print(f"tray and glyph previews in {dump}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: make_icons.py <dump-dir>")
    main(pathlib.Path(sys.argv[1]))
