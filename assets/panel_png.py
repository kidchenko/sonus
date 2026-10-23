"""Turn `sonus --render-panel <file>` output into a PNG.

    sonus --render-panel target/panel.rgba --theme dark --scale 2
    python assets/panel_png.py target/panel.rgba docs/img/panel-dark.png

The app writes raw RGBA plus a `.size` sidecar so it needs no PNG encoder of
its own; this is the encoder. Standard library only.
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from make_icons import png_rect  # noqa: E402


def main(src: pathlib.Path, dst: pathlib.Path) -> None:
    w, h = (int(v) for v in pathlib.Path(f"{src}.size").read_text().split())
    rgba = src.read_bytes()
    assert len(rgba) == w * h * 4, "size sidecar does not match the pixels"
    dst.parent.mkdir(parents=True, exist_ok=True)
    dst.write_bytes(png_rect(rgba, w, h))
    print(f"{dst} ({w}x{h})")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: panel_png.py <render.rgba> <out.png>")
    main(pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]))
