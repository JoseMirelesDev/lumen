#!/usr/bin/env python3
"""Importa el arte del artista (LibreSprite/Aseprite) a apps/lumen-slint/assets/.

Manifest-driven: lee designs/art/scenes/*.json (fuente de verdad de la escena,
docs/art-pipeline.md) y los PNG que el artista exporta a designs/art/export/,
valida tamaños y trocea sheets a assets/. El manifiesto de build (build.rs)
resuelve cada elemento a su arte real o a su placeholder al recompilar.

Contratos de export (dibujar en LibreSprite, exportar a designs/art/export/):

  Personaje por skin — sheet de 3 columnas (idle | talking | muted):
    char_<skin>.png            # 144x64  (3 x 48x64)
  Elementos de escena — según la entrada en scenes/<escena>.json:
    con "grid":                # sheet cols x (frameW x frameH) -> <id>_<n>.png
                               #   fire.png  = 1024x192 (4 x 256x192) -> brazier_0..3
    sin "grid" y con "size":   # imagen única <w>x<h> -> <id>.png
    con "fit": "fill":         # lienzo exacto design.w x design.h -> <id>.png
                               #   sky.png horizon.png floor.png props.png = 1024x576
  Sprites de partículas:       # ember.png firefly.png -> assets/scene/<sprite>

Uso:
  python3 scripts/gen-art-import.py [carpeta_de_export] [assets_dir]
  # carpetas por defecto: designs/art/export  y  apps/lumen-slint/assets
"""
import json
import sys
from pathlib import Path

from PIL import Image

REPO = Path(__file__).resolve().parent.parent
EXPORT = REPO / "designs" / "art" / "export"
ASSETS = REPO / "apps" / "lumen-slint" / "assets"
SCENES = REPO / "designs" / "art" / "scenes"

SKINS = ["ivory", "gold", "crimson", "teal", "indigo", "stone", "bronze"]
STATES = ["idle", "talking", "muted"]
CHAR_FRAME = (48, 64)     # personaje (grilla HLD: 3x la anterior 16x20)
BUST_ROWS = 48            # bust = primeras 48 filas (48x48)


def _slice(im, frame_w, frame_h, n):
    return [im.crop((i * frame_w, 0, (i + 1) * frame_w, frame_h)) for i in range(n)]


def _import_chars(src, out, errors):
    avatars = out / "avatars"
    avatars.mkdir(parents=True, exist_ok=True)
    for skin in SKINS:
        sheet = src / f"char_{skin}.png"
        if not sheet.exists():
            continue  # skin sin arte: el manifiesto sigue con el placeholder SVG
        im = Image.open(sheet)
        if im.size != (CHAR_FRAME[0] * 3, CHAR_FRAME[1]):
            errors.append(f"{sheet.name}: tamaño {im.size} != 144x64 (3 poses de 48x64)")
            continue
        for i, state in enumerate(STATES):
            frame = im.crop((i * 48, 0, (i + 1) * 48, 64))
            frame.save(avatars / f"char_{state}_{skin}.png")
            frame.crop((0, 0, 48, BUST_ROWS)).save(avatars / f"bust_{state}_{skin}.png")
        print(f"  char_{skin}: 6 PNGs (3 poses 48x64 + 3 busts 48x48)")


def _import_elements(scene, src, out, errors):
    scene_out = out / "scene"
    scene_out.mkdir(parents=True, exist_ok=True)
    design = scene.get("design", {})
    dw, dh = design.get("w", 1024), design.get("h", 576)
    for el in scene.get("elements", []):
        el_id, src_name = el.get("id"), el.get("src")
        if not el_id or not src_name:
            errors.append(f"elemento sin id/src: {el}")
            continue
        f = src / src_name
        if not f.exists():
            continue  # sin arte: build.rs cae al fallback o lo omite
        im = Image.open(f)
        grid = el.get("grid")
        if grid:
            fw, fh, cols = grid["frameW"], grid["frameH"], grid["cols"]
            if im.size != (fw * cols, fh):
                errors.append(f"{src_name}: tamaño {im.size} != {fw*cols}x{fh} ({cols} frames de {fw}x{fh})")
                continue
            for i, frame in enumerate(_slice(im, fw, fh, cols)):
                frame.save(scene_out / f"{el_id}_{i}.png")
            print(f"  {el_id} ({src_name}): {cols} frames de {fw}x{fh}")
        elif el.get("fit") == "fill":
            if im.size != (dw, dh):
                errors.append(f"{src_name}: tamaño {im.size} != {dw}x{dh} (capa a lienzo completo)")
                continue
            im.save(scene_out / f"{el_id}.png")
            print(f"  {el_id} ({src_name}): capa {dw}x{dh}")
        else:
            size = el.get("size")
            if not size:
                errors.append(f"{src_name}: elemento sin 'grid' ni 'size' — no se puede validar")
                continue
            if im.size != tuple(size):
                errors.append(f"{src_name}: tamaño {im.size} != {size[0]}x{size[1]} (size del manifiesto)")
                continue
            im.save(scene_out / f"{el_id}.png")
            print(f"  {el_id} ({src_name}): {size[0]}x{size[1]}")


def _import_particles(scene, src, out, errors):
    scene_out = out / "scene"
    scene_out.mkdir(parents=True, exist_ok=True)
    for p in scene.get("particles", []):
        sprite = p.get("sprite")
        if not sprite:
            continue
        f = src / sprite
        if not f.exists():
            continue  # placeholder de gen-pixel-assets.py
        im = Image.open(f)
        im.save(scene_out / sprite)
        print(f"  partícula {p.get('id')}: {sprite} {im.size[0]}x{im.size[1]}")


def main() -> int:
    src = Path(sys.argv[1]) if len(sys.argv) > 1 else EXPORT
    out = Path(sys.argv[2]) if len(sys.argv) > 2 else ASSETS
    if not src.is_dir():
        print(f"ERROR: no existe la carpeta de export: {src}")
        return 1
    errors = []

    _import_chars(src, out, errors)
    for scene_file in sorted(SCENES.glob("*.json")):
        scene = json.loads(scene_file.read_text())
        print(f"\nEscena {scene_file.stem}:")
        _import_elements(scene, src, out, errors)
        _import_particles(scene, src, out, errors)

    if errors:
        print("\nERRORES:")
        for e in errors:
            print(f"  - {e}")
        return 1
    print("\nArte importado. Recompila para que el manifiesto lo tome:")
    print("  cargo build -p lumen-desktop   (o cargo run -p lumen-desktop)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
