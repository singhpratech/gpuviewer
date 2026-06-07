#!/usr/bin/env python3
"""Rasterize the gpuviewer master SVG into PNGs and a Windows .ico.

Python 3 STDLIB ONLY — no Pillow, no resvg bindings. Rasterization is delegated to
the locally available headless Chrome (`google-chrome --headless --screenshot`),
which renders the SVG as a top-level document: an SVG with only a viewBox fills the
viewport, so `--window-size=N,N` yields an exact NxN raster.
`--default-background-color=00000000` keeps the background transparent.

Outputs (paths relative to the repo root, derived from this script's location):
  docs/assets/icon/png/<N>x<N>/gpuviewer.png   for N in 16/32/48/64/128/256/512
      (hicolor-shaped layout: deb/rpm install these to
       usr/share/icons/hicolor/<N>x<N>/apps/gpuviewer.png)
  docs/assets/icon/gpuviewer.ico               frames 16/24/32/48/256

ICO format choice (documented per docs/design/cross-platform.md §6): every frame is
stored PNG-COMPRESSED. PNG-compressed ICO entries have been valid for ALL sizes since
Windows Vista (Microsoft guidance historically recommended PNG for the 256 frame only
because XP could not read PNG entries; gpuviewer's Windows floor is far past that).
Storing PNG everywhere avoids hand-rolling BMP/AND-mask encoding in stdlib and keeps
the .ico small. The 24px frame exists only inside the .ico (classic Windows
small-icon metric); it is not part of the hicolor PNG set.

Usage:  python3 tools/make-icons.py
"""

import struct
import subprocess
import shutil
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SVG = REPO_ROOT / "docs" / "assets" / "icon" / "gpuviewer.svg"
PNG_DIR = REPO_ROOT / "docs" / "assets" / "icon" / "png"
ICO = REPO_ROOT / "docs" / "assets" / "icon" / "gpuviewer.ico"

PNG_SIZES = [16, 32, 48, 64, 128, 256, 512]  # hicolor set (design §6)
ICO_SIZES = [16, 24, 32, 48, 256]            # winresource frame set (design §6)

CHROME_CANDIDATES = ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"]


def find_chrome() -> str:
    for name in CHROME_CANDIDATES:
        path = shutil.which(name)
        if path:
            return path
    sys.exit("error: no headless-chrome binary found (tried: %s)" % ", ".join(CHROME_CANDIDATES))


def png_dimensions(data: bytes) -> tuple[int, int]:
    """Parse IHDR width/height — guards against device-scale-factor surprises."""
    if data[:8] != b"\x89PNG\r\n\x1a\n" or data[12:16] != b"IHDR":
        raise ValueError("not a PNG")
    w, h = struct.unpack(">II", data[16:24])
    return w, h


def rasterize(chrome: str, size: int, out: Path, scratch: Path) -> None:
    """Render SVG at exactly size x size with a transparent background."""
    out.parent.mkdir(parents=True, exist_ok=True)
    profile = scratch / f"profile-{size}"  # hermetic profile: never trips over a running Chrome
    cmd = [
        chrome,
        "--headless",
        f"--screenshot={out}",
        f"--window-size={size},{size}",
        "--default-background-color=00000000",  # transparent — icons must not ship a white plate
        "--force-device-scale-factor=1",        # exact 1 CSS px = 1 device px
        "--hide-scrollbars",
        "--disable-gpu",
        f"--user-data-dir={profile}",
        "--no-first-run",
        SVG.as_uri(),
    ]
    res = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    if res.returncode != 0 or not out.is_file():
        sys.exit(f"error: chrome rasterization failed at {size}px:\n{res.stderr}")
    w, h = png_dimensions(out.read_bytes())
    if (w, h) != (size, size):
        sys.exit(f"error: expected {size}x{size}, chrome produced {w}x{h} ({out})")


def pack_ico(frames: list[Path], out: Path) -> None:
    """Write an ICO container whose entries are raw PNG streams (Vista+ valid)."""
    images = [p.read_bytes() for p in frames]
    count = len(images)
    header = struct.pack("<HHH", 0, 1, count)  # reserved=0, type=1 (icon), count
    dir_size = 6 + 16 * count
    entries = b""
    offset = dir_size
    for data in images:
        w, h = png_dimensions(data)
        entries += struct.pack(
            "<BBBBHHII",
            w if w < 256 else 0,   # 0 encodes 256 in the u8 width/height fields
            h if h < 256 else 0,
            0,                     # palette count (none — truecolor)
            0,                     # reserved
            1,                     # color planes
            32,                    # bits per pixel (RGBA)
            len(data),
            offset,
        )
        offset += len(data)
    out.write_bytes(header + entries + b"".join(images))


def main() -> None:
    if not SVG.is_file():
        sys.exit(f"error: master SVG missing: {SVG}")
    chrome = find_chrome()
    artifacts: list[Path] = []

    with tempfile.TemporaryDirectory(prefix="gpuviewer-icons-") as tmp:
        scratch = Path(tmp)
        # hicolor PNG set.
        for size in PNG_SIZES:
            out = PNG_DIR / f"{size}x{size}" / "gpuviewer.png"
            rasterize(chrome, size, out, scratch)
            artifacts.append(out)
        # ICO frames: reuse hicolor rasters where sizes overlap; 24px goes to scratch.
        ico_frames: list[Path] = []
        for size in ICO_SIZES:
            if size in PNG_SIZES:
                ico_frames.append(PNG_DIR / f"{size}x{size}" / "gpuviewer.png")
            else:
                frame = scratch / f"gpuviewer-{size}.png"
                rasterize(chrome, size, frame, scratch)
                ico_frames.append(frame)
        pack_ico(ico_frames, ICO)
        artifacts.append(ICO)

    for path in artifacts:
        print(f"{path.stat().st_size:>8}  {path.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
