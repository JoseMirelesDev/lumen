# Vox · Diccionario HTML/CSS → Slint

Guía práctica para traducir el design system de `designs/vox-mockups/` (HTML/CSS)
al cliente Slint (`apps/lumen-slint/ui/`). Cada entrada: **concepto CSS → idioma
Slint**, con el porqué y el patrón verificado en este repo.

Fuente de autoridad: componentes en `ui/` (base.slint, voice-view.slint) — todo lo
de abajo compila y se verificó con `slint-viewer --check` / el MCP server de Slint.

---

## 1. Tokens (la fuente de la verdad)

| CSS | Slint |
|---|---|
| `:root { --vox-bg: #0C111C; }` | `export global AppTheme { in property <Theme> colors: { bg: #0C111C, ... } }` (ui/theme.slint) |
| `var(--vox-s-4)` | `AppTheme.sp-4` |
| `var(--vox-font-title)` | `AppTheme.font-display` (string de familia) |
| `--vox-pulse: 600ms` | `AppTheme.pulse` (int de ms; en Timers: `interval: AppTheme.pulse / 2 * 1ms`) |

**Regla**: ninguna vista usa literales de color/tamaño — todo referencia tokens.
Nunca declares una propiedad de componente llamada `color` (Slint prohíbe
`in property <color> color` — "Cannot override property 'color'"; usa `text-color`).

## 2. Tipografías

| CSS | Slint |
|---|---|
| `@import url("fonts.googleapis.com/...")` | `import "../fonts/Cinzel-Bold.ttf";` (pesos estáticos por archivo) |
| `font-family: var(--vox-font-body); font-weight: 600;` | `font-family: AppTheme.font-body; font-weight: 600;` |

- El parser de Slint **no acepta `@font-face`** — los fonts se declaran con
  `import "ruta.ttf"` al tope del .slint y se **embeben en el binario** con
  `EmbedResourcesKind::EmbedFiles` (build.rs).
- Los `.slint` no pueden sobreescribir `color` en un subtipo de `Text` → los
  labels son `Text` planos con estilo inline (nada de componentes wrapper:
  un wrapper `Rectangle` tiene tamaño preferido 0×0 y **colapsa dentro de
  layouts**).
- No hay `em` ni `letter-spacing` confiable en todos los backends: el look
  "mono uppercase espaciado" se logra con JetBrains Mono + mayúsculas +
  `letter-spacing: 1px`.

## 3. Layout — lo que más duele

| CSS | Slint |
|---|---|
| `display: flex; flex-direction: column;` | `VerticalLayout` / `HorizontalLayout` |
| `display: grid; grid-template-columns: ...` | `GridLayout` con `row/col` |
| `position: absolute` | `x: …; y: …` (reservar para overlays) |
| `padding` en cualquier elemento | **solo funciona en layouts** (warning: "padding only has effect on layout elements") |
| `background`/`border` en un flex | **los layouts NO pintan** background ni border — patrón inner-fill: `HorizontalLayout { Rectangle { width: 100%; height: 100%; background: …; border-…; } …contenido… }` |
| `div { height: 40px; }` | `height: 40px` **explícito** — un `Rectangle`/componente LLENA su padre por defecto; `preferred-height` se ignora en layouts |
| `justify-content: flex-start` | `alignment: start` (eje principal); `cross-axis-alignment` para el secundario. **El eje principal se comporta de forma no obvia**: con espacio extra y hijos auto-height, Slint lo distribuye — la solución robusta es altura explícita en cada hijo |

### El bug de `visible: false` (crítico)

`visible: false` **NO quita el elemento del layout** en Slint 1.17 — la fila
invisible sigue ocupando su slot (40px de hueco entre grupos). Dos fixes
verificados:

1. **Colapso a 0**: `ListRow { visible: c.kind == "text"; height: c.kind == "text" ? 40px : 0px; }` — una altura 0 contribuye 0 al layout. (Usado en channel-list.slint.)
2. **Split de modelos en Rust** (más limpio para casos grandes): filtrar en
   `src/model.rs` y exponer `text-channels`/`voice-channels` por separado.
   OJO: en este repo el binding `voice-channels` no se refrescó desde Rust
   (bug no diagnosticado) — el fix 1 es el que quedó.

## 4. Animaciones / motion pixel-step

| CSS | Slint |
|---|---|
| `@keyframes vox-campfire { … }` + `animation: vox-campfire 140ms steps(1,end) infinite` | `Timer { running: …; interval: 140ms; triggered() => { root.frame = Math.mod(root.frame + 1, 4); } }` + `Image { visible: root.frame == i; }` (swap de frames — el patrón CampfireScene) |
| `transition: all 120ms steps(2)` | `animate width { duration: 120ms; }` (solo en propiedades, dentro del elemento) |
| `animation: vox-pulse 600ms steps(2) infinite` | Timer que alterna `pulse-on` + `border-color: pulse-on ? gold : crimson` |

- Timers: **no existe `repeated: true`** — el Timer repite por defecto.
- `triggered()` (con paréntesis), no `triggered =>`.
- `animation-tick()` + blur en el renderer software = caro: usa Timers para
  animaciones continuas (el pulso, la respiración, la fogata).

## 5. Matemáticas y tipos (gotchas que cuestan un build)

| Intento | Realidad |
|---|---|
| `Math.PI` | **no existe** — literal `3.14159265358979` |
| `Math.cos(x)` con x float | espera `angle` — multiplica por `1deg`: `let ang = (…) * 1deg;` |
| `x % 4` | `Math.mod(x, 4)` (el `%` es el signo de unidad) |
| `var x = 1;` | `let x = 1;` (`var` no existe en 1.17) |
| `(q as length)` | cast no soportado así — multiplica por `1px` |
| `x as float` | `as` existe pero solo para conversiones numéricas simples |
| `string.length` | **no disponible** (adivina ancho de botón con `text == "" ? 34px : …`) |
| `x / 2` con ints | división float, truncada silenciosamente en int |

## 6. Elementos

| CSS/HTML | Slint |
|---|---|
| `<input placeholder="…">` | `TextInput` **no tiene placeholder** — overlay: `if ti.text == "" && !fs.has-focus : Text { … }` (dibujado encima; el Text no captura eventos) |
| `:focus` | `FocusScope { }` (sin `focusable`) + `fs.has-focus` para el anillo |
| `border-radius` en un elemento | OK en `Rectangle`; **los layouts no** |
| `transform: rotate(45deg)` | `transform-rotation: 45deg` (`rotation-angle` es solo de `Path`); `transform-origin: center` **no es válido** → usa `Path` con `commands: "M 4 0 L 8 4 …"` para diamantes/tails |
| `filter: brightness(0.7)` | **no hay filters** en el renderer software → overlay `Rectangle { background: #070A1280; }` |
| `image-rendering: pixelated` | `Image { image-rendering: pixelated; }` ✓ |
| `<img src>` | `Image { source: @image-url("../assets/pixel/foo.png"); }` — rutas relativas al .slint, embebidas con EmbedFiles |
| `if a : X else : Y` | **no existe `else` en elementos** — dos ifs: `if a : X` + `if !a : Y` |
| `for (const c of channels)` | `for c in root.channels:` (con índice: `for c[i] in …`) |

## 7. Verificación (flujo de desarrollo)

- Compilar rápido: `slint-viewer --check ui/foo.slint` (1.17+, sin ventana).
- Render standalone: `slint-viewer --screenshot out.png --component X ui/foo.slint`
  (dar tamaño explícito al root — si el preferred size es 0, falla con
  "window with invalid size").
- App viva: MCP server embebido:
  `SLINT_EMIT_DEBUG_INFO=1 SLINT_MCP_PORT=8080 cargo run -p lumen-desktop --features slint/mcp`
  (o `scripts/lumen-mcp.sh`). Endpoint `http://127.0.0.1:8080/mcp`:
  `list_windows` → `get_element_tree` → `click_element` / `take_screenshot`.
  Dar ids a los componentes (`channels := ChannelList { … }`) y localizar con
  `find_elements_by_id("AppWindow::channels")`.

## 8. Trabajo de arte manual pendiente

Assets actuales (placeholder generados por `scripts/gen-pixel-assets.py`):
- **Bustos y personajes**: siluetas de 1 color (palette-swap) en 7 skins.
  Arte real necesario: sprites 24×32/24×14 px multi-color (body + cape + helmet +
  accesorio, paletas indexadas de 16 colores) con poses idle / talking /
  muted_arms (boca abierta para la animación de hablar).
- **Fogata**: 4 frames recolorizados — arte real: llama animada multi-color.
- **Iconos**: Phosphor (MIT) embebido como font — nada pendiente; si se quieren
  iconos pixel-art propios, dibujar 16×16 con outline de 2px.
- **Sonidos**: kit sintetizado (`sounds/*.wav`, `scripts/gen-ui-sounds.py`) —
  reemplazable por grabaciones procesadas con el mismo carácter (cortos, <220ms).
