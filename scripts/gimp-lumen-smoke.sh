#!/usr/bin/env bash
# Smoke test del plugin GIMP 3 de Lumen (requiere el snap `gimp`).
# Crea un personaje (gold) y una escena, exporta a un dir temporal y valida
# los tamaños de los PNG. No toca assets/ del repo.
#
# Uso:  scripts/gimp-lumen-smoke.sh
#       LUMEN_SMOKE_OUT=/ruta/dir scripts/gimp-lumen-smoke.sh
set -euo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${LUMEN_SMOKE_OUT:-$HOME/lumen-art-smoke}"
rm -rf "$OUT"
mkdir -p "$OUT"/{avatars,landscape,pixel}

export LUMEN_REPO_ROOT="$DIR"
export LUMEN_SMOKE_OUT="$OUT"
timeout 150 snap run gimp -i -f --batch-interpreter python-fu-eval \
  -b "exec(open('$DIR/scripts/gimp-lumen-smoke.py').read())" 2>&1 \
  | grep -vE "WARNING|AVISO|advertencia|locale|i18n|Localization|CRITICAL|^$" || true

python3 - "$OUT" <<'EOF'
import sys
from pathlib import Path
from PIL import Image

out = Path(sys.argv[1])
exp = {"char": (16, 20), "bust": (16, 16), "landscape": (512, 288), "campfire": (128, 96)}
files = sorted(out.rglob("*.png"))
assert files, "no se exportó ningún PNG"
ok = True
for p in files:
    im = Image.open(p)
    im.load()
    if "char_" in p.name:
        e = exp["char"]
    elif "bust_" in p.name:
        e = exp["bust"]
    elif p.parent.name == "landscape":
        e = exp["landscape"]
    else:
        e = exp["campfire"]
    good = im.size == e
    ok = ok and good
    print(f"{p.relative_to(out)} {im.size} {im.mode} {'OK' if good else 'MISMATCH ' + str(e)}")
print("TODOS_OK" if ok else "HUBO_FALLOS")
sys.exit(0 if ok else 1)
EOF
