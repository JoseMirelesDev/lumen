# Pipeline de arte — Lumen (fogata + personajes)

Cómo el artista crea el paisaje de la escena de voz y los sprites/avatares de
los personajes **sin tocar código**. El artista trabaja en GIMP con capas con
nombre; un plugin exporta los PNG exactos que la app consume; el build decide
automáticamente entre arte real y placeholder.

```
designs/art/*.xcf        ← fuente del artista (GIMP, capas con nombre)
   │  scripts/gimp-lumen-export.py  (plugin, menú Lumen)
   ▼
apps/lumen-slint/assets/ ← PNGs consumidos por la UI
   │  apps/lumen-slint/build.rs  (genera ui/generated/art-manifest.slint)
   ▼
UI (Slint): Bust, Seat, CampfireScene
```

## Flujo del artista (recomendado — sin GIMP)

El pipeline es **agnóstico del editor**: dibuja en Aseprite, LibreSprite, Krita
o el que sea — lo único que importa es el PNG exportado. Aseprite (~20$ único,
o LibreSprite gratis) es el estándar de pixel-art animado: onion skin, timeline
de frames y export de sheets nativo.

1. Dibuja en tu editor (onion skin para las poses/frames).
2. Exporta los PNG con los contratos de abajo a `designs/art/export/`.
3. `python3 scripts/gen-art-import.py` — valida y trocea a `assets/`.
4. `cargo build` (o `cargo run -p lumen-desktop`): el manifiesto se regenera y
   la UI pasa a usar el arte real. Sin arte, siguen los placeholders.

El **plugin de GIMP 3** (`scripts/gimp-lumen-export.py`) sigue existiendo como
alternativa (crea los `.xcf` con capas con nombre y exporta lo mismo), pero ya
no es necesario: es un editor pensado para fotos, no para pixel-art.

## Contratos de export (lo que el script espera)

Todo en `designs/art/export/`:

| Archivo | Formato | Qué genera |
|---|---|---|
| `char_<skin>.png` | sheet **48×20** — 3 columnas: idle \| talking \| muted (16×20 c/u) | `avatars/char_{state}_{skin}.png` + `bust_{state}_{skin}.png` (bust = filas 0–15) |
| `fire.png` | sheet **512×96** — 4 frames (128×96) | `pixel/campfire_{0..3}.png` |
| `sky.png` `horizon.png` `floor.png` `props.png` | **512×288** cada una, nombre final | `landscape/*.png` |

El script valida tamaños y aborta con el detalle si algo no cuadra. Las skins
que no tengan sheet se quedan con el placeholder.

## Formatos y por qué

- La app renderiza **PNG** (estático) y **SVG** (vector, placeholders), ambos
  con `image-rendering: pixelated` para pixel art nítido.
- **No hay animación GIF/APNG/WebP**: Slint 1.17.1 no los decodifica. La
  animación de la fogata usa 4 frames (`campfire_0..3.png`) que la app alterna
  con un `Timer`. El grupo `fire` del `.xcf` de escena exporta esos 4 frames.
- `@image-url` se resuelve en tiempo de compilación: un PNG nuevo no se
  "descubre" solo. `build.rs` genera `ui/generated/art-manifest.slint`, que
  mapea cada slot al arte real si el archivo existe y al placeholder si no.
- **Sheets** (el formato de los juegos: un PNG con grilla de frames): Slint no
  trocea en runtime, así que `scripts/gen-art-import.py` lo hace en build
  (copia del editor → assets).

## Grillas (no negociables)

| Arte | Lienzo | Notas |
|---|---|---|
| Personaje (char) | 16×20 px | cuerpo completo, píxeles en la grilla |
| Bust (recorte auto) | 16×16 px | el plugin recorta las primeras 16 filas del char |
| Escena | 512×288 px | aspect 16:9, anclada abajo en la app |
| Frame de fogata | 128×96 px | 4 frames, mismo punto de anclaje |

## Capas obligatorias (nombres exactos)

**Personaje** (`designs/art/characters/<skin>.xcf`, skin en
`ivory|gold|crimson|teal|indigo|stone|bronze`):

- `char_idle` — pose en reposo
- `char_talking` — boca abierta (la app añade el pulso del borde)
- `char_muted` — boca tapada/abatida (la app añade el dim overlay)

El bust (rostro + hombros, usado en roster y lista compacta) se exporta
automáticamente como recorte de las primeras 16 filas de cada capa.

**Escena** (`designs/art/scene/campfire.xcf`), orden visual de abajo arriba:

- `sky` — fondo (atrás del todo)
- `horizon` — línea del horizonte / montañas / árboles lejanos
- `floor` — suelo, donde apoya el brasero (pintar el fogón centrado-abajo)
- `props` — elementos cercanos (troncos, piedras) que pasan por delante del suelo
- grupo `fire` — 4 capas `fire_0` … `fire_3`, 128×96, ancladas al centro-bajo
  del lienzo; son los frames animados del brasero (el plugin las exporta como
  `assets/pixel/campfire_0..3.png`, el nombre que consume la app)

> El brasero de la app ya dibuja glow/brasas sobre los frames; el artista
> pinta la estructura y las llamas. La escena se ancla abajo en la UI: el
> suelo del arte coincide con la base del brasero.
>
> ⚠️ Exporta solo cuando las capas tengan contenido: una escena vacía genera
> PNGs transparentes que **toman precedencia** sobre los gradientes actuales
> en el manifiesto de build (`has-landscape` pasa a true).

## Paleta

`designs/art/lumen.gpl` (33 colores, generada desde
`designs/vox-mockups/assets/sprites/palette.json` + bases de skin). El plugin
convierte las imágenes nuevas a modo indexado con esta paleta; GIMP impide
colores fuera de paleta en ese modo.

## Instalación del plugin

Requisito: GIMP **3.x** con Python (el snap de Canonical lo trae; Ubuntu 24.04
tiene GIMP 3.2.4 en `snap install gimp`). GIMP 3 exige que el plugin viva en
una **subcarpeta con su mismo nombre**.

```bash
# GIMP 3 vía snap (config en ~/.config/GIMP/3.2/)
mkdir -p ~/.config/GIMP/3.2/plug-ins/gimp-lumen-export
cp scripts/gimp-lumen-export.py ~/.config/GIMP/3.2/plug-ins/gimp-lumen-export/
chmod +x ~/.config/GIMP/3.2/plug-ins/gimp-lumen-export/gimp-lumen-export.py
```

La raíz del repo se detecta automáticamente (env `LUMEN_REPO_ROOT`, walk-up
desde el plugin o `~/Projects/discord-light`); si no aparece, el lienzo se
crea igual en RGB y la exportación pide la carpeta en el diálogo.

> El lienzo nuevo es de **16×20 px**: abre diminuto al 100%. Haz zoom
> (Ctrl+rueda o Vista → Zoom → 1600%) para dibujar — es normal en pixel art.

Smoke test (crea personaje + escena y exporta a `~/lumen-art-smoke`,
validando tamaños; no toca `assets/` del repo):

```bash
scripts/gimp-lumen-smoke.sh
```

Acciones del plugin (todas en **Imagen → Lumen**):

| Acción | Qué hace | Requiere lienzo abierto |
|---|---|---|
| *Nuevo personaje…* | `.xcf` 16×20 + paleta indexada + capas `char_idle/talking/muted` (ignora la imagen activa) | Sí* |
| *Nueva escena…* | `.xcf` 512×288 + capas `sky/horizon/floor/props` + grupo `fire` (ignora la imagen activa) | Sí* |
| *Exportar para Lumen…* | valida capas y exporta PNG a `apps/lumen-slint/assets/` (exporta el lienzo activo) | Sí |

\* GIMP 3 habilita los menús de plugin solo en la ventana de imagen: sin un
lienzo abierto, el menú Lumen está deshabilitado. Abre cualquier imagen
(Archivo → Nuevo, tamaño cualquiera) para usar las acciones de "nuevo" — crean
un documento aparte y puedes cerrar el lienzo de relleno.

## Cómo se resuelve en la UI

- `ui/generated/art-manifest.slint` (generado, no editar) expone
  `ArtManifest.char(skin, state)`, `ArtManifest.bust(skin, state)` y las capas
  `landscape-*` con `has-landscape`.
- `Bust` (roster/compact) y `Seat` (fogata) resuelven por skin: PNG real si
  existen los 6 archivos (`char/bust × idle/talking/muted`), si no el SVG
  placeholder.
- `CampfireScene` muestra las 4 capas de paisaje cuando `has-landscape` es
  true; sin arte, la escena queda con los gradientes actuales.

## Photoshop

El formato de capas con nombre es el mismo; se puede exportar a mano
respetando nombres y grillas. Un script JSX equivalente al plugin GIMP queda
pendiente; si el artista trabaja en Photoshop, pedirlo en el repo.

## Estado actual

- Plugin: `scripts/gimp-lumen-export.py` (GIMP 3, verificado con
  `scripts/gimp-lumen-smoke.sh` en GIMP 3.2.4 del snap — 14 PNGs, tamaños OK).
- Placeholders: `scripts/gen-svg-avatars.py` (42 SVG) y `scripts/gen-pixel-assets.py`
  (PNGs recolorizados de las siluetas maestras de `designs/vox-mockups/assets/sprites/`).
- `assets/pixel/{char,bust,seat,video_frame}_*.png` generados son huérfanos
  (la UI usa los SVG de `assets/avatars/`); no borrarlos sin verificar.
