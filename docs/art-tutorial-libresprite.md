# Tutorial: sprites para Lumen con LibreSprite

Flujo: dibujar en LibreSprite → exportar PNGs a `designs/art/export/` →
`python3 scripts/gen-art-import.py` → `cargo run`. El script valida tamaños y
trocea; la UI toma el arte automáticamente (manifiesto de build).

Contratos de export (lo que el script espera, también en `art-pipeline.md`):

| Archivo | Tamaño | Contenido |
|---|---|---|
| `char_<skin>.png` | 48×20 | personaje: 3 columnas (idle \| talking \| muted), 16×20 cada una |
| `fire.png` | 512×96 | fogata: 4 frames de 128×96 en fila |
| `sky.png` `horizon.png` `floor.png` `props.png` | 512×288 | paisaje, una capa por archivo |

---

## 0. Preparación (una vez)

1. Abre LibreSprite (`libresprite` o desde el menú de aplicaciones).
2. **Paleta de la UI** (opcional pero recomendado): la app no la impone, pero
   dibujar con los colores de `designs/art/lumen.gpl` mantiene las skins
   coherentes. Ábrela: Archivo → Abrir… → selecciona `designs/art/lumen.gpl`
   (LibreSprite la carga como paleta) y en el panel **Paleta** (Ventana →
   Paleta) → menú de la paleta → *Cargar paleta* / usa la recién abierta.
3. Cuadrícula: **Ver → Cuadrícula** (Grid) con tamaño 1 px para ver los
   píxeles. Zoom de trabajo: **1600%** (Ctrl + rueda, o Ver → Zoom).
4. Guarda los fuentes `.ase` en `designs/art/sources/` (están fuera de git).

---

## 1. Personaje (sprites/avatares)

### Lienzo
- **Archivo → Nuevo**: ancho **16**, alto **20**, fondo transparente, 3 frames
  (en el diálogo: Frames = 3).
- Regla del bust: **filas 0–15** = cabeza + hombros (es lo que se ve en el
  roster y la lista compacta; el import recorta el bust automáticamente).
  Las **filas 16–19** = piernas (solo visibles en la escena de la fogata).
  El plugin GIMP no interviene aquí: el recorte lo hace `gen-art-import.py`.

### Los 3 frames (las "poses" que la app intercambia)
| Frame | Estado | Qué dibujar |
|---|---|---|
| 1 | idle | pose en reposo, boca cerrada |
| 2 | talking | **misma pose**, boca abierta (bloque oscuro) |
| 3 | muted | **misma pose**, boca tapada (X o bufanda) |

- Usa **onion skin** (Ver → *Onion skin* / piel de cebolla) para calcar la
  pose entre frames: así los 3 comparten silueta y solo cambia la boca.
- No dibujes el borde de estado ni el dim: la app los pone encima
  (pulso crimson/ice al hablar, overlay oscuro al mutear).
- Mantén la cabeza entre las filas 0–12 y los hombros en 13–15, como los
  placeholders actuales.

### Exportar
- **Archivo → Exportar hoja de sprites…** (*Export Sprite Sheet*):
  - Columnas: **3**, Filas: **1**, Escala: **1×**
  - Capas: *Fusionada* (Merged)
  - Guarda como `designs/art/export/char_gold.png` (el skin en el nombre)
- Repite por cada skin que quieras (`char_ivory.png`, `char_crimson.png`, …).
  Truco: dibuja uno y **duplica el archivo** y recoloréalo con el panel Paleta
  (modo indexado: selecciona el color y *Reemplazar color*), o repíntalo.

---

## 2. Fuego de la fogata (animación de 4 frames)

La app anima alternando 4 frames con un Timer (Slint no decodifica GIF).

- **Archivo → Nuevo**: ancho **128**, alto **96**, transparente, **4 frames**.
- Dibuja la estructura del brasero + llamas en 4 estados: baja → media →
  alta → apagándose. Con onion skin, mantén **la base del brasero en el mismo
  píxel en los 4 frames** (si no, el fuego "baila").
- Exporta: hoja de sprites, Columnas **4**, Filas **1**, escala 1× →
  `designs/art/export/fire.png` (512×96).

> La app ya dibuja el glow/brasas detrás; tú pintas estructura + llamas. El
> fuego se posiciona centrado-abajo en la escena.

---

## 3. Paisaje de la fogata (escena)

- **Archivo → Nuevo**: ancho **512**, alto **288**, transparente, **1 frame**.
- 4 capas, de atrás adelante (panel Capas, la de abajo = fondo):
  - `sky` — cielo/noche (fondo del todo)
  - `horizon` — montañas / árboles lejanos / línea del horizonte
  - `floor` — suelo, donde apoya el brasero (**pinta el fogón centrado-abajo**)
  - `props` — elementos del primer plano (troncos, piedras) que pasan por
    delante del suelo
- La escena se ancla abajo en la app: el suelo del arte coincide con la base
  del brasero.
- **Exporta una capa por archivo** (4 exportaciones): oculta las demás capas
  (ojo de visibilidad en el panel Capas) y **Archivo → Exportar…** como
  `designs/art/export/sky.png`, `horizon.png`, `floor.png`, `props.png`.
  (Alternativa: Exportar hoja de sprites con *Capas: cada capa* y renombra los
  archivos resultantes a esos nombres.)

> ⚠️ No exportes capas vacías: PNGs transparentes **tapan** los gradientes
> actuales de la UI (tienen prioridad en el manifiesto).

---

## 4. Importar y ver en la app

```bash
# carpeta de export (crearla la primera vez)
mkdir -p designs/art/export

# valida tamaños y trocea a assets/
python3 scripts/gen-art-import.py

# verifica: 20 PNGs esperados si hay 2 skins + fuego + paisaje
# recompila y abre la app
cargo run -p lumen-desktop
```

La UI pasa del placeholder al arte real automáticamente. Si un tamaño no
cuadra, el script te dice exactamente qué archivo y qué esperaba.

---

## Trucos rápidos

- **Onion skin**: Ver → *Onion Skin* — imprescindible para poses y fuego.
- **Zoom**: Ctrl + `+` / `-`; **Cuadrícula**: Ver → Cuadrícula (1 px).
- **Recolor por skin** (modo indexado): panel Paleta → reemplazar color
  (base → otro tono de la paleta Lumen).
- **Atajos**: B = lápiz, E = borrador, G = bote, M = selección.
- Fuentes: guarda `.ase` en `designs/art/sources/`; los exports son lo único
  que el pipeline consume.
