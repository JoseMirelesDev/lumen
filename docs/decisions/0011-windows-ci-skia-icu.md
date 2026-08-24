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
  -C link-arg=/FORCE:MULTIPLE -C link-arg=/NODEFAULTLIB:icu.lib -C link-arg=/NODEFAULTLIB:icuuc.lib"`.
  Con `/FORCE:MULTIPLE` lld-link acepta el primer símbolo y descarta el resto (con warning) en vez de abortar.
  Con `/NODEFAULTLIB:icu.lib` + `/NODEFAULTLIB:icuuc.lib` se suprime el
  `/DEFAULTLIB:icu.lib`/`icuuc.lib` que Skia inyecta vía `#pragma comment(lib, ...)`
  y que hacía que el PE importara `icu.dll`/`icuuc.dll` en runtime.
  Orden de link: `skunicode_icu.lib` va primero → el binario se queda con
  el ICU **estático** y no depende de `icuuc.dll`/`icu.dll`/`icudtl.dat` en runtime (binario
  autocontenido, consistente con `embed_resources` + `embed-icudtl`).

### Update 2026-08-23 — `ucptrie_close` en runtime (artefacto solo `.exe`)

Tras `v0.4.0` el artefacto de `Windows` (`lumen-windows.exe` suelto, sin `*.dll`) fallaba al arrancar en Windows limpio:
`No se encuentra el punto de entrada ucptrie_close / icu.dll` (y `u_strFromUTF8WithSub / icuuc.dll`).

`dumpbin /DEPENDENTS` sobre `v0.4.0` (Server 2025, `windows-2025-vs2026`) mostró:
`icu.dll` → `ucptrie_close, ucptrie_get, ...` (12 símbolos) y `icuuc.dll` → `u_strFromUTF8WithSub, ...` (5).
El `skia-binaries` `0.99.0` trae `skunicode_icu.lib` **estático** (18 MB, `obj/.../icu.ucptrie.obj` define `T ucptrie_close`), pero el link inyectaba además `/DEFAULTLIB:icu.lib` y `/DEFAULTLIB:icuuc.lib` vía `#pragma comment(lib, ...)` → `lld-link` con solo `/FORCE:MULTIPLE` resolvía el duplicado `ubrk_*` al estático, pero **mantenía la entrada de import** `icu.dll/icuuc.dll` en el `PE`. El loader buscaba esas `DLL` en `System32`/`PATH` y encontraba una `ICU` antigua sin `ucptrie_close` (ICU <64, pre-Win10 1903) → error de punto de entrada.

Fix: mantener `/FORCE:MULTIPLE` y añadir `/NODEFAULTLIB:icu.lib` + `/NODEFAULTLIB:icuuc.lib` (en `RUSTFLAGS` de `release.yml`/`ci.yml` y en `.cargo/config.toml` para builds locales). Con ello `dumpbin` ya no lista `icu.dll`/`icuuc.dll` — todo `ICU` queda estático en `skunicode_icu.lib` (`U_STATIC_IMPLEMENTATION`, `embed-icudtl` ya activo vía `skia-safe`). Se añadió verificación post-link en ambos workflows:
`dumpbin /DEPENDENTS lumen.exe | findstr icu` → error si aparece.

El `zip` de `Windows` ahora es autocontenido: solo `lumen.exe` (con `icudtl.dat` embebido vía `embed-icudtl`), sin `icu.dll`/`icuuc.dll` satélite. Usuarios con `v0.4.0` deben re-descargar el nuevo artefacto; workaround temporal para `v0.4.0`: colocar junto al `.exe` un `icu.dll` + `icuuc.dll` compatibles (ICU 74, del SDK de Win11 o del `OUT_DIR/skia` del build) — no recomendado.

## Alternatives considered

- **`/FORCE:MULTIPLE` solo en release**: el debug de CI fallaba igual →
  se aplicó en ambos workflows.
- **Desactivar ICU en Skia** (`skia_use_icu=no`): rompe el text shaping
  del renderer (TEXTLAYOUT fuerza `skia_use_icu=yes`). Rechazado.
- **Parchear el build de skia-bindings para que no emita el import de
  `icuuc.dll`**: la solución limpia, pero implica parchear/vendorizar
  skia-bindings (la config de ICU del gn build). No se hizo por coste;
  queda como alternativa futura si el problema reaparece. El fix actual
  (`/NODEFAULTLIB`) es más barato y logra el mismo efecto sin vendorizar.

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
- El binario de Windows resultante no necesita `icuuc.dll`/`icu.dll`/`icudtl.dat`
  en runtime.

### Referencias

- `apps/lumen-slint/build.rs` (`run_build` en thread con 32 MiB)
- `.github/workflows/ci.yml` y `.github/workflows/release.yml`
  (`RUSTFLAGS` de los jobs de Windows)
- `.cargo/config.toml` (mismos `RUSTFLAGS` para builds locales en Windows)
- Causa raíz del renderer: `Cargo.toml` workspace
  (`renderer-winit-skia`), skia-bindings 0.99 (bundled ICU).
