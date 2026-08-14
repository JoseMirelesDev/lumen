#!/usr/bin/env python3
"""Vox pixel-art placeholder generator.

The master sprites in designs/vox-mockups/assets/sprites are single-color
silhouettes (#0C111C) meant for runtime palette-swap. This script pre-recolors
them into the app palette and writes ready-to-use PNGs under
# apps/lumen-slint/assets/pixel/. These are PLACEHOLDERS — the artist
# workflow (GIMP plugin + build manifest) is documented in docs/art-pipeline.md.

Regenerate:  python3 scripts/gen-pixel-assets.py
"""
from pathlib import Path

from PIL import Image, ImageDraw

SRC = Path(__file__).resolve().parent.parent / "designs" / "vox-mockups" / "assets" / "sprites"
OUT = Path(__file__).resolve().parent.parent / "apps" / "lumen-slint" / "assets" / "pixel"

MASTER = (0x0C, 0x11, 0x1C)  # silhouette master color

# Skin palette — one silhouette color per "character". Real multi-color art
# replaces these later; the hash->color assignment in src/model.rs stays valid.
SKINS = {
    "ivory": (0xE7, 0xE1, 0xD4),
    "gold": (0xE0, 0xB3, 0x6A),
    "crimson": (0xC1, 0x4B, 0x3F),
    "teal": (0x5A, 0xA0, 0xAA),
    "indigo": (0x8A, 0x93, 0xC4),
    "stone": (0x9A, 0x98, 0xB4),
    "bronze": (0xC9, 0xA8, 0x76),
}

# Campfire: dark structure + ivory flame -> ember-wood + gold flame
CAMPFIRE_STRUCT = (0x33, 0x26, 0x1F)
CAMPFIRE_FLAME = (0xE0, 0xB3, 0x6A)

# Seats / props
SEAT_STONE = (0x50, 0x55, 0x64)
SEAT_WOOD = (0x5A, 0x46, 0x32)
RUNE_GOLD = (0xE0, 0xB3, 0x6A)

# Video frames are pre-colored; only re-tint the talking frame's rust/amber
# to the new crimson/gold energy pair.
VIDEO_TALKING_MAP = {(0xB4, 0x43, 0x2E): (0x9E, 0x31, 0x2A), (0xE0, 0x6A, 0x3E): (0xE0, 0xB3, 0x6A)}


def recolor(im: Image.Image, mapping: dict) -> Image.Image:
    im = im.convert("RGBA")
    px = im.load()
    for y in range(im.height):
        for x in range(im.width):
            r, g, b, a = px[x, y]
            if a == 0:
                continue
            key = (r, g, b)
            if key in mapping:
                nr, ng, nb = mapping[key]
                px[x, y] = (nr, ng, nb, a)
    return im


def write(name: str, im: Image.Image) -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    im.save(OUT / name)
    print(f"  {name}")


# --- placeholders de elementos modulares (scene.json) ---------------------
# Siluetas 2-3 colores estilo HLD (contorno oscuro + madera + piedra). El
# artista reemplaza con arte real exportando el mismo nombre a designs/art/export/.

OUTLINE = (0x0A, 0x0C, 0x10)


def make_chair():
    """Silla de madera con asiento de piedra — 96x120 (diseño px)."""
    im = Image.new("RGBA", (96, 120), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    wood, stone = SEAT_WOOD, SEAT_STONE
    # respaldo
    d.rectangle([14, 8, 26, 72], fill=OUTLINE)
    d.rectangle([16, 10, 24, 70], fill=wood)
    d.rectangle([70, 8, 82, 72], fill=OUTLINE)
    d.rectangle([72, 10, 80, 70], fill=wood)
    # asiento (piedra)
    d.rectangle([8, 40, 88, 78], fill=OUTLINE)
    d.rectangle([10, 42, 86, 76], fill=stone)
    # patas
    for x in (16, 72):
        d.rectangle([x, 76, x + 8, 118], fill=OUTLINE)
        d.rectangle([x + 2, 78, x + 6, 116], fill=wood)
    return im


def make_torch(frame: int):
    """Antorcha de 2 frames (llama alta / baja) — 48x96 (diseño px)."""
    im = Image.new("RGBA", (48, 96), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    # mástil
    d.rectangle([20, 40, 28, 94], fill=OUTLINE)
    d.rectangle([22, 42, 26, 92], fill=SEAT_WOOD)
    # cabeza del mástil
    d.rectangle([16, 32, 32, 44], fill=OUTLINE)
    d.rectangle([18, 34, 30, 42], fill=SEAT_STONE)
    # llama: frame 0 alta y centrada, frame 1 baja y ladeada
    if frame == 0:
        flame = [(24, 6), (18, 20), (22, 18), (16, 30), (24, 24), (32, 30), (26, 18), (30, 20)]
        d.polygon(flame, fill=CAMPFIRE_FLAME)
        d.ellipse([20, 16, 28, 26], fill=(0xFF, 0xF5, 0xDC))
    else:
        flame = [(24, 14), (18, 26), (22, 24), (17, 34), (25, 29), (33, 32), (27, 22), (31, 24)]
        d.polygon(flame, fill=CAMPFIRE_FLAME)
        d.ellipse([21, 22, 29, 30], fill=(0xFF, 0xF5, 0xDC))
    return im


def make_ember():
    """Brasa — punto cálido 5x5 con halo."""
    im = Image.new("RGBA", (5, 5), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    d.point((2, 2), fill=(0xFF, 0xE0, 0xB0, 255))
    d.ellipse([1, 1, 3, 3], fill=(0xE0, 0xB3, 0x6A, 220))
    d.ellipse([0, 0, 4, 4], outline=(0xD1, 0x63, 0x5E, 110))
    return im


def make_firefly():
    """Luciérnaga — punto pálido 5x5 (verde ritual tenue)."""
    im = Image.new("RGBA", (5, 5), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    d.point((2, 2), fill=(0xE7, 0xFF, 0xD8, 255))
    d.ellipse([1, 1, 3, 3], fill=(0x7F, 0xC9, 0x6E, 200))
    d.ellipse([0, 0, 4, 4], outline=(0x7F, 0xC9, 0x6E, 90))
    return im


def main():
    print(f"Vox pixel placeholders -> {OUT}")

    # --- busts: 3 poses x 7 skins (24x14 logical, 72x42 px @3x) ---
    poses = {
        "idle": "characters/bust_idle.png",
        "talking": "characters/bust_talking.png",
        "muted": "characters/bust_muted_arms.png",
    }
    for pose, path in poses.items():
        base = Image.open(SRC / path)
        for skin, color in SKINS.items():
            write(f"bust_{pose}_{skin}.png", recolor(base, {MASTER: color}))

    # --- full sprites: 3 poses x 7 skins (campfire scene) ---
    full = {
        "idle": "characters/idle.png",
        "talking": "characters/talking.png",
        "muted": "characters/muted_arms.png",
    }
    for pose, path in full.items():
        base = Image.open(SRC / path)
        for skin, color in SKINS.items():
            write(f"char_{pose}_{skin}.png", recolor(base, {MASTER: color}))

    # --- campfire: 4 frames (structure + flame) ---
    for i in range(4):
        base = Image.open(SRC / "campfire" / f"frame_{i}.png")
        write(
            f"campfire_{i}.png",
            recolor(base, {MASTER: CAMPFIRE_STRUCT, (0xFF, 0xF5, 0xDC): CAMPFIRE_FLAME}),
        )

    # --- seats / props ---
    write("seat_log.png", recolor(Image.open(SRC / "ui" / "log_seat.png"), {MASTER: SEAT_WOOD}))
    write("seat_stone.png", recolor(Image.open(SRC / "ui" / "stone_seat.png"), {MASTER: SEAT_STONE}))
    write("rune_gold.png", recolor(Image.open(SRC / "ui" / "rune.png"), {MASTER: RUNE_GOLD}))

    # --- elementos modulares (scene.json): sillas, antorchas, partículas ---
    write("chair.png", make_chair())
    write("torch_0.png", make_torch(0))
    write("torch_1.png", make_torch(1))
    write("ember.png", make_ember())
    write("firefly.png", make_firefly())

    # --- video frames (re-tint talking variant) ---
    idle = Image.open(SRC / "ui" / "video_frame.png")
    write("video_frame.png", idle.convert("RGBA"))
    write("video_frame_talking.png", recolor(Image.open(SRC / "ui" / "video_frame_talking.png"), VIDEO_TALKING_MAP))

    print("done.")


if __name__ == "__main__":
    main()
