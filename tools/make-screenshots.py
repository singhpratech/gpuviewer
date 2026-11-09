#!/usr/bin/env python3
"""Render the README screenshots (docs/assets/replay.svg, timeline.svg) from the real TUI.

Stdlib only. Two steps, both reproducible:

  1. Dump the real `ui::render` output for the seeded 8h demo recording, cell by cell
     (symbol, fg, bg, modifiers), with the on-demand test in crates/tui/src/app.rs:

       GPV_SHOTS_DIR=/tmp/shots \\
         cargo test -p gpuviewer-tui --bin gpuviewer -- --ignored screenshot_dump

  2. Turn those dumps into SVG terminal windows (GitHub-dark palette, monospace grid):

       python3 tools/make-screenshots.py /tmp/shots docs/assets

Nothing is drawn by hand: every glyph and color comes from the buffer the TUI rendered.
The data is the demo's simulated recording, and the TUI labels it "(mock)" itself.
"""

import json
import sys
from pathlib import Path
from xml.sax.saxutils import escape

CHAR_W = 8.4
ROW_H = 18
X0 = 16.0
Y0 = 46.0
BASELINE = 13.5

FG_DEFAULT = "#c9d1d9"
BG_DEFAULT = "#0d1117"
PALETTE = {
    "Reset": None,
    "Black": "#0d1117",
    "Red": "#ff7b72",
    "Green": "#3fb950",
    "Yellow": "#d29922",
    "Blue": "#58a6ff",
    "Magenta": "#bc8cff",
    "Cyan": "#39c5cf",
    "Gray": "#8b949e",
    "DarkGray": "#6e7681",
    "LightRed": "#ffa198",
    "LightGreen": "#56d364",
    "LightYellow": "#e3b341",
    "LightBlue": "#79c0ff",
    "LightMagenta": "#d2a8ff",
    "LightCyan": "#56d4dd",
    "White": "#f0f6fc",
}

# Block-element glyphs drawn as filled rectangles instead of font glyphs: fonts disagree on
# their metrics, and glyph-drawn blocks leave seams and stray ticks between rows. Value =
# (fraction of the cell filled, measured from the bottom; True for the top half).
BLOCKS = {
    "█": (1.0, False),
    "▇": (7 / 8, False),
    "▆": (6 / 8, False),
    "▅": (5 / 8, False),
    "▄": (4 / 8, False),
    "▃": (3 / 8, False),
    "▂": (2 / 8, False),
    "▁": (1 / 8, False),
    "▀": (4 / 8, True),
}

TITLES = {
    "replay": "gpuviewer demo — scroll-back replay at the throttle onset",
    "timeline": "gpuviewer demo — timeline (t) at 1h: an hour per screen, every stall and "
    "throttle on the event lane; unrecorded time stays blank",
}


def color(name, default):
    c = PALETTE.get(name)
    if c is None and name not in PALETTE:
        raise SystemExit(f"unmapped color {name!r} — add it to PALETTE")
    return c or default


def cell_style(cell):
    _sym, fg, bg, mods = cell
    fgc = color(fg, FG_DEFAULT)
    bgc = PALETTE.get(bg)
    if "REVERSED" in mods:
        fgc, bgc = (bgc or BG_DEFAULT), fgc
    return fgc, bgc, "BOLD" in mods, "UNDERLINED" in mods, "ITALIC" in mods


def render(rows, title):
    cols, nrows = len(rows[0]), len(rows)
    w = round(cols * CHAR_W + 32)
    h = nrows * ROW_H + 68
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" '
        f'viewBox="0 0 {w} {h}" font-family="ui-monospace,SFMono-Regular,Menlo,'
        f'&quot;DejaVu Sans Mono&quot;,Consolas,monospace" font-size="14px">',
        f'<rect width="{w}" height="{h}" rx="10" fill="#161b22" stroke="#30363d"/>',
        '<circle cx="22" cy="18" r="6" fill="#ff5f57"/>',
        '<circle cx="42" cy="18" r="6" fill="#febc2e"/>',
        '<circle cx="62" cy="18" r="6" fill="#28c840"/>',
        f'<text x="{w / 2:.0f}" y="22" text-anchor="middle" fill="#8b949e" '
        f'font-size="12px">{escape(title)}</text>',
        f'<rect x="8" y="38" width="{w - 16}" height="{h - 52}" rx="6" fill="{BG_DEFAULT}"/>',
    ]
    texts = []
    for y, row in enumerate(rows):
        top = Y0 + y * ROW_H
        # Background runs.
        x = 0
        while x < cols:
            _fg, bg, *_ = cell_style(row[x])
            if bg is None:
                x += 1
                continue
            start = x
            while x < cols and cell_style(row[x])[1] == bg:
                x += 1
            out.append(
                f'<rect x="{X0 + start * CHAR_W:.1f}" y="{top:.1f}" '
                f'width="{(x - start) * CHAR_W:.1f}" height="{ROW_H}" fill="{bg}"/>'
            )
        # Block glyphs as rectangles, merged across same-glyph same-color runs.
        x = 0
        while x < cols:
            sym = row[x][0]
            if sym not in BLOCKS:
                x += 1
                continue
            fg = cell_style(row[x])[0]
            start = x
            while x < cols and row[x][0] == sym and cell_style(row[x])[0] == fg:
                x += 1
            frac, top_half = BLOCKS[sym]
            hgt = ROW_H * frac
            ry = top if top_half else top + ROW_H - hgt
            out.append(
                f'<rect x="{X0 + start * CHAR_W:.1f}" y="{ry:.1f}" '
                f'width="{(x - start) * CHAR_W:.1f}" height="{hgt:.2f}" fill="{fg}"/>'
            )
        # Foreground runs: same style, contiguous; leading/trailing blanks trimmed.
        # Block glyphs were drawn above, so they read as blanks here.
        row = [[" " if c[0] in BLOCKS else c[0], *c[1:]] for c in row]
        x = 0
        while x < cols:
            style = cell_style(row[x])
            start = x
            while x < cols and cell_style(row[x]) == style:
                x += 1
            text = "".join(c[0] for c in row[start:x])
            stripped = text.rstrip(" ")
            lead = len(stripped) - len(stripped.lstrip(" "))
            stripped = stripped.lstrip(" ")
            if not stripped:
                continue
            fg, _bg, bold, under, italic = style
            n = len(stripped)
            attrs = ""
            if bold:
                attrs += ' font-weight="600"'
            if under:
                attrs += ' text-decoration="underline"'
            if italic:
                attrs += ' font-style="italic"'
            texts.append(
                f'<text x="{X0 + (start + lead) * CHAR_W:.1f}" y="{top + BASELINE:.1f}" '
                f'fill="{fg}" textLength="{n * CHAR_W:.1f}" lengthAdjust="spacingAndGlyphs" '
                f'xml:space="preserve"{attrs}>{escape(stripped)}</text>'
            )
    out.extend(texts)
    out.append("</svg>")
    return "\n".join(out) + "\n"


def main():
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    for name, title in TITLES.items():
        rows = json.loads((src / f"{name}.json").read_text())
        (dst / f"{name}.svg").write_text(render(rows, title))
        print(f"wrote {dst / f'{name}.svg'} ({len(rows[0])}x{len(rows)} cells)")


if __name__ == "__main__":
    main()
