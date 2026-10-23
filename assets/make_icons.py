"""Turn `sonus --dump-icons <dir>` output into the committed icon assets.

    sonus --dump-icons target/icons
    python assets/make_icons.py target/icons

Writes, next to this script:
  icon.ico       every size up to 256, PNG-compressed entries (Vista+)
  icon-256.png   for the Chocolatey/winget listings and the README
  icon-1024.png  the master build.sh turns into the macOS .icns

and a PNG of every tray face and glyph into the dump directory, for looking
at.

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

    (HERE / "icon.ico").write_bytes(ico([(e, png(load("app", e), e)) for e in ICO_SIZES]))
    for edge in (256, 1024):
        (HERE / f"icon-{edge}.png").write_bytes(png(load("app", edge), edge))

    for pattern in ("tray-*.rgba", "glyph-*.rgba"):
        for path in sorted(dump.glob(pattern)):
            edge = int(path.stem.rsplit("-", 1)[1])
            path.with_suffix(".png").write_bytes(png(path.read_bytes(), edge))

    print(f"wrote icon.ico, icon-256.png, icon-1024.png to {HERE}")
    print(f"tray and glyph previews in {dump}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: make_icons.py <dump-dir>")
    main(pathlib.Path(sys.argv[1]))
