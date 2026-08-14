#!/usr/bin/env python3
"""Lumen — plugin de GIMP 3 para el arte de la UI (Python via GI).

El artista dibuja en GIMP con capas con nombre; este plugin exporta los PNG
exactos que la app consume (apps/lumen-slint/assets/*) sin que el artista
piense en nombres de archivo, tamaños ni rutas. La app resuelve el arte real
frente a los placeholders en build time (apps/lumen-slint/build.rs).

Tres acciones (menú Imagen -> Lumen). GIMP 3 expone los menús de plugin solo
en la ventana de imagen y exige un lienzo abierto para habilitarlos; las
acciones de "nuevo" ignoran la imagen activa y crean un documento aparte:
  · Nuevo personaje…   crea un .xcf 16x20 con paleta Lumen indexada y las
                        capas char_idle / char_talking / char_muted
  · Nueva escena…      crea un .xcf 512x288 con las capas sky / horizon /
                        floor / props y el grupo fire (4 frames 128x96)
  · Exportar para Lumen… valida las capas y exporta PNGs a
                        apps/lumen-slint/assets/ (exporta el lienzo activo)

Instalación (GIMP 3.x — p.ej. el snap `gimp`, config en ~/.config/GIMP/3.2/):
  mkdir -p ~/.config/GIMP/3.2/plug-ins/gimp-lumen-export
  cp scripts/gimp-lumen-export.py ~/.config/GIMP/3.2/plug-ins/gimp-lumen-export/
  chmod +x ~/.config/GIMP/3.2/plug-ins/gimp-lumen-export/gimp-lumen-export.py
(GIMP 3 exige que el plugin viva en una subcarpeta con su mismo nombre.)

La raíz del repo se detecta desde la ubicación del plugin (../..) o con la
variable de entorno LUMEN_REPO_ROOT.

Spec completa: docs/art-pipeline.md
"""
import os
import sys
import time
from pathlib import Path

import gi

gi.require_version("Gimp", "3.0")
gi.require_version("Gegl", "0.4")
gi.require_version("Gio", "2.0")
from gi.repository import Gimp, Gegl, Gio, GLib, GObject

# --- convenciones de la grilla (docs/art-pipeline.md) --------------------
CHAR_COLS, CHAR_ROWS = 16, 20   # personaje de cuerpo completo (grid pixel)
BUST_ROWS = 16                  # bust = recorte de las primeras 16 filas
SCENE_W, SCENE_H = 512, 288     # lienzo de la escena de fogata
FIRE_W, FIRE_H = 128, 96        # cada frame de la fogata

SKINS = ["ivory", "gold", "crimson", "teal", "indigo", "stone", "bronze"]
STATES = ["idle", "talking", "muted"]
SCENE_LAYERS = ["sky", "horizon", "floor", "props"]
FIRE_FRAMES = ["fire_0", "fire_1", "fire_2", "fire_3"]

PALETTE_NAME = "Lumen"
CHAR_LAYERS = ["char_%s" % s for s in STATES]


# --- helpers --------------------------------------------------------------
_REPO_MARKER = os.path.join("designs", "art", "lumen.gpl")


def _is_repo(path):
    return os.path.isfile(os.path.join(path, _REPO_MARKER))


def _repo_root():
    """Raíz del repo: env LUMEN_REPO_ROOT, o walk-up desde el plugin, o rutas
    comunes del home. Devuelve None si no la encuentra (el flujo continúa en
    RGB sin paleta y la exportación pide la carpeta en el diálogo)."""
    root = os.environ.get("LUMEN_REPO_ROOT")
    if root and _is_repo(root):
        return root
    try:
        d = os.path.dirname(os.path.abspath(__file__))
        while True:
            if _is_repo(d):
                return d
            parent = os.path.dirname(d)
            if parent == d:
                break
            d = parent
    except NameError:
        pass
    for cand in ("~/Projects/discord-light", "~/discord-light", "~/lumen", "~/Projects/lumen"):
        p = os.path.expanduser(cand)
        if _is_repo(p):
            return p
    return None


def _palette_path():
    return os.path.join(_repo_root(), "designs", "art", "lumen.gpl")


def _read_palette_colors():
    """Colores de designs/art/lumen.gpl como [(r, g, b), ...]."""
    colors = []
    with open(_palette_path()) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#") or line.startswith("GIMP"):
                continue
            parts = line.split()
            if len(parts) >= 3 and all(p.isdigit() for p in parts[:3]):
                colors.append((int(parts[0]), int(parts[1]), int(parts[2])))
    return colors


def _index_image(image):
    """Convierte la imagen a modo indexado con la paleta Lumen (CUSTOM).

    Si la paleta no se puede leer/crear/poblar, deja la imagen en RGB y avisa
    — la exportación funciona igual; la paleta es una ayuda de disciplina.
    NUNCA aborta el flujo: el lienzo se crea igualmente.
    """
    try:
        colors = _read_palette_colors()
    except Exception as e:
        colors = None
        Gimp.message("Lumen: no se pudo leer la paleta (%s) — %s. Imagen en RGB." % (_palette_path(), e))
    if not colors:
        return
    try:
        # Nombre único por run: la paleta queda en el DB de GIMP y un nombre
        # repetido resolvería a la entrada stale (vacía) de un run anterior.
        pal_name = "%s-%d" % (PALETTE_NAME, int(time.time()))
        pal = Gimp.Palette.new(pal_name)
        ok = True
        for r, g, b in colors:
            res, _ = pal.add_entry("%02x%02x%02x" % (r, g, b), Gegl.Color.new("rgb(%d,%d,%d)" % (r, g, b)))
            ok = ok and res
        if not ok:
            raise RuntimeError("entradas de paleta rechazadas")
        image.convert_indexed(
            Gimp.ConvertDitherType.FS,
            Gimp.ConvertPaletteType.CUSTOM,
            len(colors),
            True, True, pal_name,
        )
    except Exception as e:
        Gimp.message("Lumen: no se pudo indexar con la paleta (%s) — imagen en RGB." % e)


def _new_image(w, h):
    image = Gimp.Image.new(w, h, Gimp.ImageBaseType.RGB)
    for layer in list(image.get_layers()):
        image.remove_layer(layer)
    return image


def _new_layer(image, name, w, h, parent=None):
    layer = Gimp.Layer.new(image, name, w, h, Gimp.ImageType.RGBA_IMAGE, 100.0, Gimp.LayerMode.NORMAL)
    image.insert_layer(layer, parent, 0)
    return layer


def _show(image):
    try:
        Gimp.Display.new(image)
    except Exception as e:
        Gimp.message("Lumen: el lienzo se creó pero no se pudo abrir la ventana (%s). "
                     "Búscalo en Ventanas." % e)


def _visible_path(layer):
    """Cadena de capas de la raíz al drawable (para mantenerlas visibles)."""
    path = []
    while layer is not None:
        path.append(layer)
        layer = layer.get_parent()
    return path


def _png(image, drawable, path):
    """Exporta un drawable como PNG, recortando al rect del drawable cuando
    es más chico que el lienzo (p.ej. los frames fire de 128x96 en la escena
    de 512x288). GIMP 3 no tiene file-png-save* en el PDB y Gimp.file_save
    exporta la imagen completa: se ocultan las capas ajenas y se restaura.
    """
    dw, dh = drawable.get_width(), drawable.get_height()
    iw, ih = image.get_width(), image.get_height()
    if (dw, dh) == (iw, ih):
        _save_drawable(image, drawable, path)
        return
    ox, oy = 0, 0
    try:
        ox, oy = drawable.get_offsets()
    except Exception:
        pass
    dup = image.duplicate()
    try:
        dup.crop(dw, dh, ox, oy)
        d = dup.get_layer_by_name(drawable.get_name())
        if d is None:
            raise RuntimeError("capa %s no encontrada en el duplicado" % drawable.get_name())
        _save_drawable(dup, d, path)
    finally:
        dup.delete()


def _save_drawable(image, drawable, path):
    """Guarda la imagen con solo el drawable visible (Gimp.file_save)."""
    keep = set(_visible_path(drawable))
    hidden = []
    for l in image.get_layers():
        if l not in keep:
            hidden.append((l, l.get_visible()))
            l.set_visible(False)
    parent = drawable.get_parent()
    if parent is not None:
        for sib in parent.get_children():
            if sib is not drawable:
                hidden.append((sib, sib.get_visible()))
                sib.set_visible(False)
    try:
        ok = Gimp.file_save(Gimp.RunMode.NONINTERACTIVE, image, Gio.File.new_for_path(path), None)
    finally:
        for l, v in hidden:
            l.set_visible(v)
    if not ok:
        raise RuntimeError("No se pudo exportar %s" % path)


def _get_layer(image, name):
    try:
        return image.get_layer_by_name(name)
    except Exception:
        return None


# --- creadores ------------------------------------------------------------
def _make_character(skin):
    image = _new_image(CHAR_COLS, CHAR_ROWS)
    try:
        image.set_file(Gio.File.new_for_path(os.path.join(
            _repo_root(), "designs", "art", "characters", "%s.xcf" % skin)))
    except Exception:
        pass
    for state in STATES:
        _new_layer(image, "char_%s" % state, CHAR_COLS, CHAR_ROWS)
    try:
        _index_image(image)
    except Exception as e:
        Gimp.message("Lumen: indexación omitida (%s) — imagen en RGB." % e)
    _show(image)
    Gimp.message("Lumen: personaje %s creado (16x20, %d capas: %s). "
                 "Se abrió como documento nuevo '[Sin nombre]-N.0' — haz zoom a 1600%% para dibujar."
                 % (skin, len(CHAR_LAYERS), " / ".join(CHAR_LAYERS)))
    return image


def _make_scene():
    image = _new_image(SCENE_W, SCENE_H)
    try:
        image.set_file(Gio.File.new_for_path(os.path.join(
            _repo_root(), "designs", "art", "scene", "campfire.xcf")))
    except Exception:
        pass
    for name in SCENE_LAYERS:
        _new_layer(image, name, SCENE_W, SCENE_H)
    group = Gimp.GroupLayer.new(image, "fire")
    image.insert_layer(group, None, 0)
    for name in FIRE_FRAMES:
        _new_layer(image, name, FIRE_W, FIRE_H, parent=group)
    try:
        _index_image(image)
    except Exception as e:
        Gimp.message("Lumen: indexación omitida (%s) — imagen en RGB." % e)
    _show(image)
    Gimp.message("Lumen: escena creada (512x288, capas %s + grupo fire). "
                 "Se abrió como documento nuevo '[Sin nombre]-N.0' — el fuego va centrado-abajo."
                 % " / ".join(SCENE_LAYERS))
    return image


# --- exportadores ---------------------------------------------------------
def _export_character(image, skin, out_dir):
    avatars = os.path.join(out_dir, "avatars")
    if not os.path.isdir(avatars):
        raise RuntimeError("No existe el directorio %s" % avatars)
    missing = [n for n in CHAR_LAYERS if _get_layer(image, n) is None]
    if missing:
        raise RuntimeError("Faltan capas del personaje: %s" % ", ".join(missing))
    exported = []
    for state in STATES:
        layer = _get_layer(image, "char_%s" % state)
        full = os.path.join(avatars, "char_%s_%s.png" % (state, skin))
        _png(image, layer, full)
        exported.append(full)
        # bust: duplicado recortado a las primeras BUST_ROWS filas
        dup = image.duplicate()
        try:
            dlayer = _get_layer(dup, "char_%s" % state)
            dup.crop(CHAR_COLS, BUST_ROWS, 0, 0)
            bust = os.path.join(avatars, "bust_%s_%s.png" % (state, skin))
            _png(dup, dlayer, bust)
            exported.append(bust)
        finally:
            dup.delete()
    return exported


def _export_scene(image, out_dir):
    landscape = os.path.join(out_dir, "landscape")
    pixel = os.path.join(out_dir, "pixel")
    os.makedirs(landscape, exist_ok=True)
    os.makedirs(pixel, exist_ok=True)
    missing = [n for n in SCENE_LAYERS if _get_layer(image, n) is None]
    if missing:
        raise RuntimeError("Faltan capas de la escena: %s" % ", ".join(missing))
    exported = []
    for name in SCENE_LAYERS:
        layer = _get_layer(image, name)
        path = os.path.join(landscape, "%s.png" % name)
        _png(image, layer, path)
        exported.append(path)
    group = _get_layer(image, "fire")
    if group is not None:
        try:
            for child in group.get_children():
                name = child.get_name()
                if name in FIRE_FRAMES:
                    # la app consume campfire_<n>.png (voice-view.slint)
                    idx = name.split("_")[1]
                    path = os.path.join(pixel, "campfire_%s.png" % idx)
                    _png(image, child, path)
                    exported.append(path)
        except Exception:
            pass
    return exported


def _export(image, kind, skin, out_dir):
    out_dir = os.path.abspath(out_dir)
    if not os.path.isdir(out_dir):
        raise RuntimeError("El directorio de salida no existe: %s" % out_dir)
    if kind == "personaje":
        return _export_character(image, skin, out_dir)
    return _export_scene(image, out_dir)


def _choice_value(config, prop, options, default):
    """Lee un argumento choice del config: el valor es el nick de la opción."""
    v = config.get_property(prop)
    return v if v in options else default


# --- plugin GIMP 3 ---------------------------------------------------------
class LumenPlugin(Gimp.PlugIn):
    def do_query_procedures(self):
        return ["lumen-new-character", "lumen-new-scene", "lumen-export"]

    def _attribution(self, proc, name, blurb):
        proc.set_menu_label(blurb)
        proc.set_documentation(blurb, "Arte de la UI de Lumen (docs/art-pipeline.md)", name)
        proc.set_attribution("Lumen", "Lumen", "2026")

    def _skins_choice(self):
        choice = Gimp.Choice.new()
        for i, s in enumerate(SKINS):
            choice.add(s, i, s, "")
        return choice

    def do_create_procedure(self, name):
        # GIMP 3 solo admite menús sobre imagen (<Image>, <Layers>, …) — no hay
        # <Toolbox>. Las tres acciones se montan en Imagen -> Lumen; las de
        # "nuevo" ignoran la imagen activa.
        if name == "lumen-new-character":
            # GIMP 3 solo expone menús de plugin dentro de la ventana de
            # imagen y exige la firma estándar de image procedure para todo
            # lo que viva en el menubar: aunque "nuevo" no use la imagen
            # activa, se registra como ImageProcedure (sensitivity por
            # defecto -> requiere un lienzo abierto para estar habilitado).
            proc = Gimp.ImageProcedure.new(self, name, Gimp.PDBProcType.PLUGIN, self.run_new_character)
            self._attribution(proc, name, "Nuevo personaje…")
            proc.add_menu_path("<Image>/Image/Lumen")
            proc.set_image_types("RGB*, GRAY*, INDEXED*")
            proc.add_choice_argument("skin", "Skin", None, self._skins_choice(), "ivory", GObject.ParamFlags.READWRITE)
            return proc
        if name == "lumen-new-scene":
            proc = Gimp.ImageProcedure.new(self, name, Gimp.PDBProcType.PLUGIN, self.run_new_scene)
            self._attribution(proc, name, "Nueva escena…")
            proc.add_menu_path("<Image>/Image/Lumen")
            proc.set_image_types("RGB*, GRAY*, INDEXED*")
            return proc
        if name == "lumen-export":
            proc = Gimp.ImageProcedure.new(self, name, Gimp.PDBProcType.PLUGIN, self.run_export)
            self._attribution(proc, name, "Exportar para Lumen…")
            proc.add_menu_path("<Image>/Image/Lumen")
            proc.set_image_types("RGB*, GRAY*, INDEXED*")
            kinds = Gimp.Choice.new()
            kinds.add("personaje", 0, "personaje", "")
            kinds.add("escena", 1, "escena", "")
            proc.add_choice_argument("kind", "Tipo", None, kinds, "personaje", GObject.ParamFlags.READWRITE)
            proc.add_choice_argument("skin", "Skin (solo personaje)", None, self._skins_choice(), "ivory", GObject.ParamFlags.READWRITE)
            default_dir = Gio.File.new_for_path(os.path.join(_repo_root(), "apps", "lumen-slint", "assets"))
            proc.add_file_argument("out_dir", "Carpeta de salida (apps/lumen-slint/assets)", None,
                                   Gimp.FileChooserAction.SELECT_FOLDER, True, default_dir, GObject.ParamFlags.READWRITE)
            return proc
        return None

    def _done(self, procedure, message=None):
        if message:
            Gimp.message(message)
        return procedure.new_return_values(Gimp.PDBStatusType.SUCCESS, GLib.Error())

    def _fail(self, procedure, error):
        Gimp.message("Lumen: error — %s" % error)
        return procedure.new_return_values(Gimp.PDBStatusType.EXECUTION_ERROR, GLib.Error())

    def run_new_character(self, procedure, run_mode, image, drawables, config):
        try:
            skin = _choice_value(config, "skin", SKINS, "ivory")
            _make_character(skin)
            return self._done(procedure)
        except Exception as e:
            return self._fail(procedure, e)

    def run_new_scene(self, procedure, run_mode, image, drawables, config):
        try:
            _make_scene()
            return self._done(procedure)
        except Exception as e:
            return self._fail(procedure, e)

    def run_export(self, procedure, run_mode, image, drawables, config):
        try:
            kind = _choice_value(config, "kind", ["personaje", "escena"], "personaje")
            skin = _choice_value(config, "skin", SKINS, "ivory")
            out_file = config.get_property("out_dir")
            out_dir = out_file.get_path() if out_file is not None else None
            if not out_dir:
                raise RuntimeError("Falta la carpeta de salida")
            exported = _export(image, kind, skin, out_dir)
            rel = "\n".join(os.path.relpath(p, out_dir) for p in exported)
            return self._done(procedure, "Exportados a %s:\n%s" % (out_dir, rel))
        except Exception as e:
            return self._fail(procedure, e)


if __name__ == "__main__":
    Gimp.main(LumenPlugin.__gtype__, sys.argv)
