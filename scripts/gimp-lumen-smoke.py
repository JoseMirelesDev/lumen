"""Smoke test del plugin GIMP 3 de Lumen — se ejecuta DENTRO del python de GIMP.

Uso (desde scripts/gimp-lumen-smoke.sh):
  snap run gimp -i -f --batch-interpreter python-fu-eval \
    -b 'exec(open("<repo>/scripts/gimp-lumen-smoke.py").read())'

Crea un personaje (gold) y una escena, exporta a LUMEN_SMOKE_OUT (o
~/lumen-art-smoke) y valida tamaños/capas. No toca assets/ del repo.
"""
import os

ns = {"__name__": "lumen_smoke"}
root = os.environ.get("LUMEN_REPO_ROOT", os.path.dirname(os.path.abspath(__file__)))
exec(open(os.path.join(root, "scripts", "gimp-lumen-export.py")).read(), ns)

out = os.environ.get("LUMEN_SMOKE_OUT", os.path.expanduser("~/lumen-art-smoke"))
for d in ("avatars", "landscape", "pixel"):
    os.makedirs(out + "/" + d, exist_ok=True)

img = ns["_make_character"]("gold")
print("CHAR_LAYERS:", [l.get_name() for l in img.get_layers()])
print("CHAR_INDEXED:", img.get_base_type())
ns["_export"](img, "personaje", "gold", out)

img2 = ns["_make_scene"]()
print("SCENE_LAYERS:", [l.get_name() for l in img2.get_layers()])
print("FIRE_CHILDREN:", [c.get_name() for c in img2.get_layer_by_name("fire").get_children()])
ns["_export"](img2, "escena", "ivory", out)

print("TEST_DONE")
ns["Gimp"].quit()
