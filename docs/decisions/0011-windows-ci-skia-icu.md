# ADR 0011 — Windows CI: stack del build script + `/FORCE:MULTIPLE` para el ICU de Skia

Status: accepted · Date: 2026-08-14 · Scope: `apps/lumen-slint` build + `.github/workflows/{ci,release}.yml`

## Context

El renderer pasó de `renderer-winit-software` a `renderer-winit-skia` (Cargo.toml
workspace). En Windows el build de release/CI fallaba con dos errores:

1. **`STATUS_STACK_OVERFLOW` (0xc00000fd)** en el build script de
   `lumen-desktop`: el compilador de Slint (`slint_build`) recursiona sobre
   el AST de los `.slint` grandes (chat-view con overlay de miembros,
   settings con tabs). El hilo main de Windows tiene stack de 1 MiB
   (Linux/macOS: 8 MiB), por eso solo falla en Windows.
2. **`lld-link: duplicate symbol` (ubrk_*, ures_*, uloc_*, u_charsToUChars…)**:
   skia-bindings bundlea ICU en Windows **dos veces** — objetos estáticos
   dentro de `skunicode_icu.lib` **y** un import de `icuuc.dll`. Ambos
   definen los mismos ~20 símbolos → link abortado. Es un quirk del build
   de skia-bindings 0.99 en Windows (en Linux se linkea el ICU del sistema
   vía pkg-config, una sola vez).

## Decision

- `apps/lumen-slint/build.rs`: todo el build corre en un thread con
  **stack_size(32 MiB)** (`main()` → `run_build()` en un thread + `join`).
  Aplica a las 3 plataformas (el envuelve es incondicional).
- Windows (ci.yml + release.yml): `RUSTFLAGS="-C linker=lld-link
  -C link-arg=/FORCE:MULTIPLE"`. Con `/FORCE:MULTIPLE` lld-link acepta el
  primer símbolo y descarta el resto (con warning) en vez de abortar.
  Orden de link: `skunicode_icu.lib` va primero → el binario se queda con
  el ICU **estático** y no depende de `icuuc.dll` en runtime (binario
  autocontenido, consistente con `embed_resources`).

## Alternatives considered

- **`/FORCE:MULTIPLE` solo en release**: el debug de CI fallaba igual →
  se aplicó en ambos workflows.
- **Desactivar ICU en Skia** (`skia_use_icu=no`): rompe el text shaping
  del renderer (TEXTLAYOUT fuerza `skia_use_icu=yes`). Rechazado.
- **Parchear el build de skia-bindings para que no emita el import de
  `icuuc.dll`**: la solución limpia, pero implica parchear/vendorizar
  skia-bindings (la config de ICU del gn build). No se hizo por coste;
  queda como alternativa futura si el problema reaparece.

## Consequences

### Riesgo (importante para desarrollo futuro)

`/FORCE:MULTIPLE` es un martillo: **acepta CUALQUIER definición duplicada**,
no solo las de ICU. Si en el futuro dos dependencias embeben la misma
librería C (o dos versiones de una), el linker tomará la primera en
silencio y la colisión **no abortará el build** — el bug sería sutil (un
crate ejecutando el código de la otra versión). Verificado hoy: los únicos
duplicados son los ~20 símbolos de ICU de Skia (copias idénticas).

**Qué vigilar al añadir dependencias**:
- Dependencias C/C++ embebidas (bundled): sherpa-onnx, onnxruntime,
  webrtc, skia — cualquiera que compile una lib nativa. Si dos comparten
  símbolos (p.ej. dos copias de `zlib`, `libpng`, `opus`, ICU), el link de
  Windows no avisará por build fallido.
- El warning de `/FORCE:MULTIPLE` queda en el log del job de Windows —
  revisarlo al cambiar deps del voice stack o del renderer.
- Si un binario de Windows nuevo muestra comportamiento raro en texto
  (shaping/ICU) o en audio (libs duplicadas), sospechar primero de un
  duplicado silenciado.

### Qué NO afecta

- El binario runtime no cambia su stack (el fix del thread es solo del
  compilador). Linux/macOS no llevan la flag y su link es estricto.
- El binario de Windows resultante no necesita `icuuc.dll`/`icudtl.dat`
  en runtime.

### Referencias

- `apps/lumen-slint/build.rs` (`run_build` en thread con 32 MiB)
- `.github/workflows/ci.yml` y `.github/workflows/release.yml`
  (`RUSTFLAGS` de los jobs de Windows)
- Causa raíz del renderer: `Cargo.toml` workspace
  (`renderer-winit-skia`), skia-bindings 0.99 (bundled ICU).
