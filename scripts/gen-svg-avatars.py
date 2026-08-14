#!/usr/bin/env python3
"""Generate PIXEL-ART character avatars as SVG — one distinct character per
skin, with idle/talking/muted states, in bust and full-body crops.

The design system is pixel art (image-rendering: pixelated everywhere), but
rendered as SVG rects on an integer grid the art stays crisp at ANY scale —
fixing the "expanded pixel blob" feeling without leaving the pixel identity.
The app icons are separate SVG (scripts/gen-svg-icons.py); this script is
only for the character identity art.

Output: assets/avatars/{bust,char}_{idle,talking,muted}_{skin}.svg

Run: python3 scripts/gen-svg-avatars.py
"""
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "apps/lumen-slint/assets/avatars"

# palette keys: B=base A=accent F=face D=dark L=light .=transparent
SKINS = {
    "ivory":  {"B": "#C9C2B0", "A": "#E7E1D4", "F": "#E8D0B0", "D": "#3A3F52", "L": "#F5F2EA"},
    "gold":   {"B": "#D9B36C", "A": "#F0CA7E", "F": "#E8C8A0", "D": "#4A3A20", "L": "#F7E2B8"},
    "crimson": {"B": "#9E312A", "A": "#D1635E", "F": "#D8A888", "D": "#2A1618", "L": "#E8A098"},
    "teal":   {"B": "#3C6E78", "A": "#5AA0AA", "F": "#C8A888", "D": "#10262B", "L": "#9CD0DA"},
    "indigo": {"B": "#2A3458", "A": "#8FA3C9", "F": "#D0B898", "D": "#12162B", "L": "#C4CFF0"},
    "stone":  {"B": "#8A8A96", "A": "#C8C8D4", "F": "#E0B890", "D": "#26262E", "L": "#E0E0EA"},
    "bronze": {"B": "#8A7355", "A": "#C9A876", "F": "#D8B090", "D": "#241D12", "L": "#E8C890"},
}

# --- heads: 12 rows x 16 cols --------------------------------------------
# knight: closed great helm with visor
HEAD_KNIGHT = [
    "....BBBBBBBB....",
    "..BBBBBBBBBBBB..",
    ".BBBBBBBBBBBBBB.",
    ".BBBDDDDDDDDBBB.",
    ".BBBDDLLWDDDBBB.",
    ".BBBDDDDDDDDBBB.",
    ".BBBBBBBBBBBBBB.",
    ".BBBBBBBBBBBBBB.",
    "..BBBBBBBBBBBB..",
    "...BBBBBBBBBB...",
    "....BBBBBBBB....",
    "....BBBBBBBB....",
]
# berserker: open face, horns, braids
HEAD_BERSERKER = [
    "AA..BBBBBBBB..AA",
    ".A..BBBBBBBB..A.",
    "....BBBBBBBB....",
    "...BFFBBBBFFB...",
    "..BFFFFBBFFFFB..",
    "..BFFDDDDDDFB...",
    "..BFFDLLDDDFB...",
    "..BFFDDDDDDFB...",
    "...BFFDDFFDB....",
    "....BFFDDFB.....",
    "....BBBBBBBB....",
    "...BBBBBBBBBB...",
]
# warlord: spiked crown, war paint
HEAD_WARLORD = [
    "...A........A...",
    "....A..AA..A....",
    ".....AAAAAA.....",
    "..AAAAAAAAAAAA..",
    ".BFFFFFFFFFFFFB.",
    ".BFFDDDDDDDDFFB.",
    ".BFFDLLLLDDDFB..",
    ".BFFDDDDDDDDFB..",
    ".BFDDAADDDDDFB..",
    "..BFFDDDDDDFB...",
    "...BFFFFFFFB....",
    "....BBBBBBBB....",
]
# ranger: hood + mask
HEAD_RANGER = [
    "..AAAAAAAAAAAA..",
    ".AAABBBBBBBBAAA.",
    "AABBBBBBBBBBBBAA",
    "ABBBBBBBBBBBBBBA",
    "ABFFFFFFFFFFFBBA",
    "ABFFDDDDDDDDFFBA",
    "ABFFDLLDLLDDFFBA",
    "ABFFDDDDDDDDFFBA",
    "ABBBDDDDDDDDBBBA",
    ".AABBBBBBBBBBAA.",
    "..A.BBBBBBBB.A..",
    "....BBBBBBBB....",
]
# wizard: pointed hat + scarf
HEAD_WIZARD = [
    "........A.......",
    ".......AA.......",
    "......AAA.......",
    ".....AAAA.......",
    "....AAAAAA......",
    "...AAAAAAAA.....",
    "..AAAAAAAAAAA...",
    ".BFFBBBBBBBFFB..",
    ".BFFDDDDDDDFFB..",
    ".BFFDLLDDDDFB...",
    ".BFFDDDDDDDFB...",
    ".BBBAAAAAAAA.B..",
]
# dwarf: round helm + big beard
HEAD_DWARF = [
    "....BBBBBBBB....",
    "..BBBBBBBBBBBB..",
    ".BBBBBBBBBBBBBB.",
    ".BBBBAAAAAABBBB.",
    ".BBBAAAAAAABBBB.",
    "..BBFFFFFFFBB...",
    "..BFFDDDDDDFB...",
    "..BFFDLLDDDFB...",
    "..BFFDDDDDDFB...",
    "..BBBBBBBBBBB...",
    "..BBB.BBBB.BB...",
    "...BBBBBBBBB....",
]
# legion: crest + plume
HEAD_LEGION = [
    ".....AAAAAAAA....",
    ".....AAAAAAAA....",
    "....BBBBBBBBBB...",
    "...BBBBBBBBBBBB..",
    "..BFFBBBBBBBFFB..",
    "..BFFDDDDDDDFFB..",
    "..BFFDLLDDDDFB...",
    "..BFFDDDDDDDFB...",
    "..BFFAAAAAADFB...",
    "...BFFFFFFFB....",
    "....BBBBBBBB....",
    "....BBBBBBBB....",
]

HEADS = {
    "ivory": HEAD_KNIGHT, "gold": HEAD_BERSERKER, "crimson": HEAD_WARLORD,
    "teal": HEAD_RANGER, "indigo": HEAD_WIZARD, "stone": HEAD_DWARF,
    "bronze": HEAD_LEGION,
}

# shared body: 4 rows shoulders (bust) + 8 rows torso/legs (char)
SHOULDERS = [
    ".BBBBBBBBBBBBBB.",
    "BBBBBBBBBBBBBBBB",
    "BBBBBBBBBBBBBBBB",
    "BBBBBBBBBBBBBBBB",
]
TORSO = [
    ".BBBBBBBBBBBBBB.",
    "BBBBBBBBBBBBBBBB",
    "BAAAAAAAAAAAAAAB",
    "BBBBBBBBBBBBBBBB",
    ".BB.BBBBBBBB.BB.",
    "..BBBBBBBBBBBB..",
    "...BBBB..BBBB...",
    "...DDDD..DDDD...",
    "...DDDD..DDDD...",
    "...DDDD..DDDD...",
]

def apply_state(head, state):
    """State overlay on the mouth area (rows 6-7):
    - talking: open the mouth (dark block) where the face is;
    - muted: X slash across the mouth."""
    rows = [list(r) for r in head]
    if state == "talking":
        r = rows[6]
        if "F" in r[4:12]:
            for c in (6, 7, 8, 9):
                r[c] = "D"
    elif state == "muted":
        for c in (6, 9):
            rows[6][c] = "D"
        for c in (7, 8):
            rows[7][c] = "D"
    return rows


def rows_to_svg(rows, pal, scale=1):
    """Emit pixel rows as SVG rects (merged per horizontal run)."""
    parts = []
    for y, row in enumerate(rows):
        x = 0
        while x < len(row):
            ch = row[x]
            if ch == ".":
                x += 1
                continue
            run = 1
            while x + run < len(row) and row[x + run] == ch:
                run += 1
            color = pal.get(ch)
            if color:
                parts.append(f'<rect x="{x}" y="{y}" width="{run}" height="1" fill="{color}"/>')
            x += run
    w = max(len(r) for r in rows)
    h = len(rows)
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" shape-rendering="crispEdges">\n' + "\n".join(parts) + "\n</svg>\n"


def build(kind, skin, state, pal):
    head = apply_state(HEADS[skin], state)
    if kind == "bust":
        rows = head + SHOULDERS
    else:
        rows = head + TORSO
    return rows_to_svg(rows, pal)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for skin, pal in SKINS.items():
        for state in ("idle", "talking", "muted"):
            for kind in ("bust", "char"):
                (OUT / f"{kind}_{state}_{skin}.svg").write_text(build(kind, skin, state, pal))
    print(f"generados {len(SKINS) * 3 * 2} avatares pixel-SVG en {OUT}")


if __name__ == "__main__":
    main()
