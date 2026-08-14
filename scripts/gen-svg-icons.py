#!/usr/bin/env python3
"""Download the app's UI icons as official Phosphor SVGs (MIT,
phosphor-icons/core, regular weight) and normalize them for Slint
(`fill=#000000`; the `.slint` side tints via `Image { colorize }`).

Maps `Icons.<name>` in icons.slint to the Phosphor icon name via the
embedded selection.json (codepoint -> name). Offline fallback: extract the
outlines from the embedded Phosphor.ttf instead.

Run: python3 scripts/gen-svg-icons.py
"""
import json
import re
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FONT = ROOT / "apps/lumen-slint/fonts/Phosphor.ttf"
SELECTION = ROOT / "apps/lumen-slint/fonts/Phosphor-selection.json"
ICONS_SLINT = ROOT / "apps/lumen-slint/ui/icons.slint"
OUT = ROOT / "apps/lumen-slint/assets/icons"
PHOSPHOR_URL = "https://raw.githubusercontent.com/phosphor-icons/core/main/assets/regular/{}.svg"


def phosphor_name(codepoint: int) -> str | None:
    sel = json.loads(SELECTION.read_text())
    for ic in sel["icons"]:
        cp = ic["properties"]["code"]
        if isinstance(cp, int):
            cp = f"{cp:x}"
        if cp.lower() == f"{codepoint:x}":
            return ic["properties"].get("name")
    return None


def download(name: str, cp: int) -> bytes | None:
    pname = phosphor_name(cp)
    if not pname:
        print(f"  [skip] {name}: sin nombre Phosphor para {hex(cp)}")
        return None
    url = PHOSPHOR_URL.format(pname)
    with urllib.request.urlopen(url, timeout=20) as r:
        svg = r.read().decode()
    # normalize: neutral fill (colorize tints later); keep official path
    svg = svg.replace('fill="currentColor"', 'fill="#000000"')
    return svg.encode()


def extract_from_font(name: str, cp: int) -> bytes | None:
    """Fallback: pull the glyph outline out of the embedded TTF."""
    from fontTools.pens.boundsPen import BoundsPen
    from fontTools.pens.svgPathPen import SVGPathPen
    from fontTools.ttLib import TTFont

    font = TTFont(str(FONT))
    cmap = font.getBestCmap()
    glyphset = font.getGlyphSet()
    gname = cmap.get(cp)
    if gname is None:
        return None
    pen = SVGPathPen(glyphset)
    glyphset[gname].draw(pen)
    path = pen.getCommands()
    bpen = BoundsPen(glyphset)
    glyphset[gname].draw(bpen)
    xmin, ymin, xmax, ymax = bpen.bounds
    svg = (
        f'<svg xmlns="http://www.w3.org/2000/svg" '
        f'viewBox="{xmin:.1f} 0 {xmax - xmin:.1f} {ymax - ymin:.1f}">\n'
        f'  <g transform="translate(0 {ymax:.1f}) scale(1 -1)">\n'
        f'    <path fill="#000000" fill-rule="evenodd" d="{path}"/>\n'
        f"  </g>\n"
        f"</svg>\n"
    )
    return svg.encode()


def main() -> None:
    src = ICONS_SLINT.read_text()
    used = {
        m.group(1): int(m.group(2), 16)
        for m in re.finditer(r'in property <string> ([\w-]+): "\\u\{([0-9A-Fa-f]+)\}"', src)
    }
    OUT.mkdir(parents=True, exist_ok=True)
    ok = failed = 0
    for name, cp in sorted(used.items()):
        data = None
        try:
            data = download(name, cp)
        except Exception as e:  # noqa: BLE001 — offline or 404 -> fallback
            print(f"  [download fallo] {name}: {e}")
        if data is None:
            try:
                data = extract_from_font(name, cp)
            except Exception as e:  # noqa: BLE001
                print(f"  [font fallo] {name}: {e}")
        if data is None:
            failed += 1
            continue
        (OUT / f"{name}.svg").write_bytes(data)
        ok += 1
    print(f"iconos: {ok} ok, {failed} fallidos")
    if failed:
        sys.exit(1)


if __name__ == "__main__":
    main()
