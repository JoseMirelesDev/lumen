# Deep dive: por qué no funciona la cancelación de eco — 2026-08-27

> Investigación cruzada: código propio (`lumen-voice`, post-migración a Sonora) vs. implementaciones OSS que SÍ funcionan (Chromium/libwebrtc, PulseAudio, PipeWire, pjproject, Linphone/mswebrtc, Mumble, TeamTalk, Jami, Telegram/tg_owt, echo-aec). Todas las citas externas verificadas en código fuente real (clones/fetch 2026-08-27); las internas verificadas con lectura directa de `client.rs`/`audio.rs`.

## TL;DR — la causa raíz

**El feed de render tiene dos invariantes que AEC3 exige y que lumen-voice viola: continuidad (un bloque de 10 ms por cada bloque de captura, sin huecos) y orden (render siempre antes que su captura). El gate RMS drena los chunks silenciosos sin alimentarlos → agujeros en la timeline del render → el estimador de delay diverge → el filtro adaptativo nunca converge → el supresor trata la voz local como eco.**

Ninguna de las implementaciones de referencia gatea el silencio. Ninguna. Chromium/WebRTC solo *detecta* actividad del render (`DetectActiveRender`) para estadística — nunca salta la inserción. PulseAudio rellena con silencio si falta playback. PipeWire alimenta incondicionalmente con rings pre-inicializados en cero. pjproject alimenta **ceros incluso con el EC suspendido**. Linphone inyecta ceros ("Not enough ref samples, using zeroes"). Jami hace `fillWithSilence`. Mumble, TeamTalk y echo-aec alimentan siempre.

Los otros hallazgos (cap que descarta sin reset, tap vacío sin padding, sin delay hint, NS VeryHigh enmascarando la métrica) son agravantes o síntomas, no causas raíz.

---

## 1. Estado actual del código (verificado)

Flujo de producción (`client.rs:440-618`, `audio.rs:704-722, 998-1063`):

```
[cpal output callback, thread RT]
  drain_into: mezcla post-mix pre-device → tap_resampler (linear, dev→48k) → render_tap (Arc<Mutex<Vec<i16>>>)
[tokio send loop, por frame de mic de 20 ms = 960]
  cap 300 ms: si tap.len() > 14400 → drain(..excess) silencioso (descarta el más viejo)   [client.rs:541-545]
  available = floor(tap.len()/480)*480; to_feed = min(available, 960)
  render = tap.drain(..to_feed)          ← DRENADO INCONDICIONAL                          [client.rs:556]
  por cada chunk de 480:
    rms > 0.0008 → ns.process_render_frame(chunk)   ← SOLO si pasa el gate              [client.rs:562-568]
    rms ≤ 0.0008 → (drenado pero NO alimentado)  ← AGUJERO EN LA TIMELINE
  ns.process(960 captura) → Sonora AEC3 → WebRTC NS VeryHigh + HPF + GC2 → FE → opus
```

Hechos verificados con lectura directa:

- **F1** `client.rs:556` drena ANTES del gate de `client.rs:563` — el chunk silencioso se pierde para siempre (`skipped_silence` es solo diag).
- **F2** El comentario del propio código (`client.rs:561`) documenta la evidencia: con gate 0.01 "**fed=0 tap 2k**" (voz real bloqueada, tap acumulando 40 ms), con 0.002 "still blocked 30% (1888 fed0 vs 2880 fed960)". La respuesta fue bajar el umbral 0.01→0.002→0.0008 en lugar de eliminar el gate.
- **F3** Tap vacío → `to_feed = 0` → **cero render alimentado** para ese frame de captura, pero `process_capture` sigue corriendo.
- **F4** Cap 300 ms con drop silencioso del más viejo, sin reset del AEC (`client.rs:535-545`). El comentario dice "fix Larsen delay 224 >150" — el estimador midió 224 ms y se subió el cap.
- **F5** `audio.rs:1040-1042`: `chunks_exact(480)` descarta el trailing <480 en silencio y `let _ =` traga errores de `process_render_i16`.
- **F6** `audio.rs:1199-1202`: con `aec_enabled=false` el render NO se alimenta (no-op) y el tap se limpia — el síntoma histórico "corrompe independiente de aec_enabled" era del código pre-Sonora (WebRTC APM); en webrtc upstream `ProcessReverseStream` con `echo_controller==nullptr` es inerte (`audio_processing_impl.cc:1595-1625`), así que no aplica al código actual.
- **F7** Sin `set_stream_delay_ms` ni estimación externa de delay en ningún punto.
- **F8** Contención: `render_tap` es `parking_lot::Mutex` tomado por el callback RT de output (`audio.rs:719`) y por el send loop (`client.rs:534`).

## 2. Qué hacen los que funciona (evidencia por implementación)

### Chromium / libwebrtc — el patrón canónico

- **Render se alimenta PRE-playout** desde el propio render path (`AudioProcessor::OnPlayoutData` → `AnalyzePlayoutData` → `AnalyzeReverseStream`, `media/webrtc/audio_processor.cc`), antes de escribir al device. El delay se computa cada frame: `set_stream_delay_ms((t_render - t_analyze) + (t_process - t_capture))`.
- **Sin gate**: `EchoCanceller3::AnalyzeRender` → `RenderDelayBuffer::Insert` se llama por CADA bloque; `DetectActiveRender` solo actualiza un contador de actividad, no salta la inserción.
- **Qué pasa dentro de AEC3 con feeds rotos** (esto es la clave del diagnóstico, `modules/audio_processing/aec3/render_delay_buffer.cc`, `block_processor.cc`):
  - Hueco (Insert no llamado): `render_call_counter` no avanza mientras `capture_call_counter` sí → delay implícito se infla monótonamente; el matched filter correlaciona captura contra ceros → delay estimado oscila → `echo_path_variability.delay_change = kNewDetectedDelay` continuo → el filtro se re-alinea sin parar y **nunca converge**.
  - Render tarde/vacío: `RenderUnderrun()` → `delay_ -= 1` y avanza read sin consumir → eco no cancelado + divergencia.
  - Exceso sostenido (>8 bloques durante 1 s): `DetectExcessRenderBlocks()` → `Reset()` + `kBufferFlush` → se pierde convergencia y vuelve a `initial_state_seconds` (2.5 s) sin supresión efectiva.

### PulseAudio `module-echo-cancel` (webrtc.cc)

- Feed del reverse **siempre**: `do_push()` hace `pa_memblockq_peek_fixed_size` y si `plen < sink_blocksize` **avanza el puntero rellenando silencio** — nunca salta `ProcessReverseStream` (`module-echo-cancel.c:814-830`).
- Referencia: sink virtual, post-mix pre-device (equivalente al tap de lumen — este punto está BIEN).
- Todo el AEC corre en UN hilo (source thread) con el play encolado vía `asyncmsgq` desde el sink thread; orden garantizado play→record por bloque.
- `set_stream_delay_ms(0)`, drift resuelto fuera del APM (drop/resync por `calc_diff`).

### PipeWire `aec-webrtc`

- `webrtc_run()` itera `ProcessReverseStream → set_stream_delay_ms((blocks-1)*10) → ProcessStream` **incondicional** (`aec-webrtc.cpp:376-390`); rings pre-inicializados en cero.
- El graph solo procesa cuando `capture_cycle == sink_cycle` — sincronía estructural, no FIFO best-effort.
- Overflow: dropea bloques viejos SOLO para mantener play alineado con capture (`pavail > avail → read_update`), nunca como cap arbitrario.
- Buffer máximo 100 ms, no 300.

### pjproject (PJSIP)

- `play_cb` alimenta `pjmedia_echo_playback` con CADA frame; con underrun sostiene "EC suspended" pero **sigue alimentando ceros** para no perder el delay (`sound_port.c:98-180`).
- Delay hint = `output_latency_ms * 3/4` explícito; drift absorbido por `pjmedia_delay_buf` (WSOLA), fuera del APM.
- Tail default 200 ms. Comentario clave (`echo_webrtc.c:178-189`): *"A poor estimate, even by as little as 40ms, may affect the echo cancellation results greatly"* — y aún así prefieren delay-agnostic + alimentación continua.

### Linphone / mswebrtc

- Si falta referencia: inyecta ceros con warning `"Not enough ref samples, using zeroes"` (`aec.c:195-210`). Flow controller purga exceso de referencia cada 5 s — balance activo, nunca huecos.

### Mumble

- `Resynchronizer` con lag nominal 2 frames (~20 ms): **retrasa el mic a propósito** para garantizar que el speaker-data preceda al mic-data (cita del manual de Speex en `AudioInput.h:56`). Cola de 5 slots con drops controlados para mantener fill 2-4.
- Siempre `addEcho` si hay canal de eco; sin gate.

### Jami

- `fillWithSilence` cuando no hay playback que mezclar (`audiolayer.cpp getToPlay`); `putRecorded` **solo procesa si `playbackStarted && recordStarted`** — no procesa captura sin referencia disponible.
- `tidyQueues()` dropea playback y record **balanceados** (no solo un lado).
- Drift explícito: `set_stream_drift_samples(playbackQueue.samples - recordQueue.samples)`.

### Telegram / tg_owt

- Usa el ADM completo de libwebrtc: `ProcessReverseStream` desde el callback de playout ANTES del HAL, delay desde `AudioDeviceBuffer` (`GetPlayoutDelay`), locks separados render/capture, `SwapQueue` de 100 frames sin drop.

### echo-aec (keybodhi — el único otro usuario real del crate tonarino en Rust)

- `Arc<Processor>` compartido, DOS threads desacoplados: loopback → `process_render_frame`, mic → `process_capture_frame`. **Sin FIFO manual, sin cap, sin gate, `stream_delay_ms: None`**. Su README lo dice explícito: *"No gestionar manualmente el offset/delay de la señal de referencia — entra en conflicto con la estimación interna de AEC3"*.

### Jamulus

- Sin AEC por diseño: exige audífonos. Lección de UX: cuando el AEC no puede garantizarse, la restricción de uso es la solución honesta.

---

## 3. Diagnóstico: cada síntoma explicado

### Síntoma A — "AEC aporta -0.7 dB"

Dos capas:

1. **Artefacto de medición**: el probe mide post-NS VeryHigh, que ya suprime -22 dB solo. El aporte real del AEC (NS off) es **-13.5 dB** — medido y consistente con un AEC3 mal alimentado (lo esperable en webrtc con feed correcto: -25 a -40 dB).
2. **Contribución real limitada** por RC1-RC4 (abajo). El combinado -25 dB "suena a silencio" pero el AEC per se no está trabajando.

### Síntoma B — "alimentar cualquier render corrompe la voz (corr 0.6→0.04), incluso casi-silencio"

**RC1 (causa raíz): el gate RMS agujerea la timeline del render.** El mecanismo exacto, desde el código de AEC3 (ChromiumCanonical):

> Saltar un bloque de render = no llamar `Insert` → el `DownsampledRenderBuffer` tiene ceros donde había señal → el `EchoPathDelayEstimator` (matched filter) pierde la correlación → `estimated_delay_` oscila → `AlignFromDelay` cambia de bloque cada vez → `echo_path_variability.delay_change = kNewDetectedDelay` continuo → el filtro adaptativo se re-alinea sin parar y el supresor residual, con un filtro divergente, suprime la voz near-end.

Y el caso "casi-silencio" es el PEOR para el gate, no el más benigno: los chunks near-silenciosos pasan el umbral 0.0008 de forma esporádica → timeline con huecos irregulares máximo → máxima divergencia. Por eso "cualquier render corrompe": alimentar render en este diseño ES alimentar una timeline agujereada. La evidencia propia del repo (F2) lo corrobora: el gate bloqueó voz real completa ("fed=0 tap 2k") y al 30% con 0.002.

Nota histórica: el claim del diary 2026-08-09 "independiente de aec_enabled" era del código pre-Sonora; hoy con `aec_enabled=false` no se alimenta nada (F6) y en webrtc upstream el reverse con AEC off es inerte. El síntoma vigente es con AEC on.

### Síntoma C — "Larsen persiste en mic_test"

**RC3: el delay medido de 224 ms es un síntoma, no una propiedad física.** Una cadena WASAPI compartida + sala tiene 10-40 ms de path acústico+HAL. Que el estimador reporte 224 ms (y que "arreglarlo" fuera subir el cap 150→300 ms, F4) indica que estaba midiendo el offset acumulado por la timeline agujereada: si por cada 20 ms de captura se alimentan menos de 20 ms de render, el delay implícito crece monótonamente — exactamente el modo de fallo documentado en `render_delay_buffer.cc`. Con RC1/RC2 corregidos, el delay estimado debe colapsar a la decena de ms y el filtro puede alinear el eco real. El anti-howling (`anti_howling_gain`) ni siquiera es alcanzable hoy: `EchoCanceller3Config::default()` está hardcodeado dentro de sonora (doc en `audio.rs:974-997`) — parche secundario solo si después del fix de feed persiste.

### Agravantes

- **RC2**: tap vacío → cero feed ese frame (F3) → `RenderUnderrun` → `delay_ -= 1` → divergencia adicional. Todos los de referencia hacen padding de ceros o no procesan la captura sin referencia.
- **RC4**: drop-oldest silencioso al superar 300 ms sin reset (F4) — deja el filtro alineado a una timeline desplazada; AEC3 nativo ante overrun hace `Reset()` explícito. Un cap de 300 ms además admite 300 ms de referencia stale.
- **RC7**: trailing <480 descartado por `chunks_exact` (F5) — deriva de longitud si el resampler lineal del tap produce longitudes no múltiplo; errores tragados con `let _`.
- **RC8**: `Mutex` de parking_lot en el callback RT (F8) — riesgo de xrun/priority inversion; los de referencia usan colas lock-free o mensajería entre threads.

## 4. Plan de fix priorizado

### P0 — restaurar la invariante de feed (chico, localizado en `client.rs`)

1. **Eliminar el gate RMS** (`client.rs:560-570`): alimentar CADA chunk de 480 drenado, sea silencio o no. Borrar la rama `skipped_silence`.
2. **Padding en vacío**: si `to_feed == 0` (tap vacío o <480), alimentar `vec![0i16; 960]` (2×480) antes de procesar la captura — patrón PulseAudio/pjproject/Linphone. Alternativa válida: patrón Jami (no procesar captura sin referencia) pero rompe el flujo del chain; el padding de ceros es el estándar.
3. **Overflow → reset explícito**: al superar el cap, además del drop, recrear el `SonoraAec` (el send loop ya reconstruye `NoiseSuppressor` en otros paths — mismo mecanismo). Estado fresco > estado divergente. Cap puede bajar a 100-150 ms una vez el delay real se mida.
4. Alinear el trailing: drain con `chunks(480)` + acumular el resto al frente del tap en vez de descartarlo (o garantizar múltiplo en `resample_into`).

Espera tras P0: `get_stats().delay_ms` colapsa de ~224 ms a 10-40 ms y se estabiliza; ERLE sube; la corrupción de voz con render alimentado desaparece (AEC3 convergido es transparente con audífonos — verificado en ChromiumCanonical §6).

### P1 — medición honesta

5. **Reescribir `aec_cancel` contra Sonora replicando el feed de producción** (tap + padding de ceros + batching 20 ms): el probe actual está `#[ignore]`-ado como inválido para Sonora y no replica el feed (era single-thread lockstep perfecto — justo el caso que SÍ funcionaba). Métricas: Goertzel pre-NS (o NS=None), ERLE de `get_stats()`, y corr de voz on/off. Sin esto no hay gate objetivo para P0.
6. Loguear `delay_ms`/ERLE/ERLE por segundo en el probe (ya existe `suppression_trend` como molde).

### P2 — solo si tras P0 queda síntoma

7. **Delay hint**: computar `(frames en tap + latencia de output) * 1000 / 48000` y pasarlo como `stream_delay_ms` inicial; fórmula Chromium `(t_render - t_analyze) + (t_process - t_capture)`. Con timeline limpia el auto-estimador de AEC3 basta (Chromium por defecto solo usa el hint como verificación), así que esto es opcional.
8. **Parche sonora-aec3** para `EchoCanceller3Config` (el tuning ya documentado en `audio.rs:988-997`: anti_howling 200/0.3, DTD enr 0.35/snr 20, headroom 64) — SOLO si Larsen o sobre-supresión persisten con feed correcto. Tunear antes del fix de feed es lo que se hizo hasta ahora y no funcionó.
9. **Lock-free ring** (`rtrb`/`ringbuf`) para `render_tap` — sacar el Mutex del callback RT.
10. **NS High en medición** (default de PipeWire) — VeryHigh solo para producción.

### No-fixes (decisiones ya correctas)

- Tap post-mix pre-device en el callback de output: mismo punto que PipeWire (`sink_process` clona el mix antes del driver). Correcto.
- `stream_delay_ms: None` (auto): correcto por defecto; echo-aec lo usa igual.
- Toggle AEC por usuario + headphones-first: correcto (Mumble/Jamulus validan la UX).
- Lockstep 1:1 render:capture en bloques de 480: correcto en espíritu (PA `do_push` intercala igual); el defecto era el gate/cap, no la estructura.

## 5. Riesgos del P0

- Alimentar ceros de forma continua con audífonos: AEC3 con render continuo silencioso converge a filtro nulo (transparente) — es el caso Chromium por defecto; no hay daño documentado. El riesgo histórico "cualquier render corrompe" era la timeline agujereada, no el render en sí.
- CPU: `process_render_frame` por frame extra en silencio es despreciable (APM ~140 µs/frame, medido en dev-diary 2026-08-12).

## 6. Fuentes

Propias: `client.rs:440-618`, `audio.rs:704-722, 974-1063, 1198-1206, 2100+`, `tests/aec_cancel.rs`, `local/aec-fix-sketch.md`, `local/aec-reference.md`.

Externas (verificadas en fuente 2026-08-27):
- Chromium `media/webrtc/audio_processor.cc` (`OnPlayoutData`/`AnalyzePlayoutData`/`ProcessData` — delay y tap pre-playout) — chromium.googlesource.com/chromium/src
- webrtc `modules/audio_processing/aec3/{render_delay_buffer,block_processor,echo_canceller3}.cc`, `audio_processing_impl.cc`, `api/audio/audio_processing.h` (invariantes de Insert/underrun/overrun, inercia con AEC off, fórmula de delay) — webrtc.googlesource.com/src
- PulseAudio `src/modules/echo-cancel/{webrtc.cc,module-echo-cancel.c}` (padding de silencio, single-thread, delay 0) — gitlab.freedesktop.org/pulseaudio/pulseaudio
- PipeWire `spa/plugins/aec/aec-webrtc.cpp`, `src/modules/module-echo-cancel.c` (feed incondicional, cycle-sync, rings) — gitlab.freedesktop.org/pipewire/pipewire
- pjproject `pjmedia/src/pjmedia/{echo_webrtc.c,echo_webrtc_aec3.cpp,echo_common.c,sound_port.c,delaybuf.c}` (ceros en suspensión, WSOLA, tail 200, latency 3/4) — github.com/pjsip/pjproject
- Linphone `mswebrtc aec.c` (inyección de ceros, flow controller) — github.com/Linphone-sync/mswebrtc (mirror)
- Mumble `src/mumble/{AudioInput.cpp,AudioInput.h,WASAPI.cpp}` (Resynchronizer lag 2, loopback, sin gate) — github.com/mumble-voip/mumble
- TeamTalk5 `Library/TeamTalkLib/avstream/WebRTCPreprocess.cpp`, `SoundLoopback.cpp` (delay DacTime−AdcTime, sin gate) — github.com/BearWare/TeamTalk5
- Jami `src/media/audio/{audiolayer.cpp,audio-processing/webrtc.cpp,audio-processing/audio_processor.h}` (fillWithSilence, tidyQueues balanceado, drift samples) — github.com/savoirfairelinux/jami-daemon
- tg_owt `src/modules/audio_processing/*`, `src/audio/audio_transport_impl.cc` (ADM, orden, SwapQueue) — github.com/desktop-app/tg_owt
- echo-aec `audio-core/src/audio/{aec.rs,engine.rs}` (Arc<Processor>, 2 threads, sin FIFO/gate — el patrón Rust más cercano) — github.com/keybodhi/echo-aec
- Jamulus `src/clientdlg.cpp`, `linux/Jamulus.1` (audífonos obligatorios, sin AEC) — github.com/jamulussoftware/jamulus

Hallazgo negativo relevante: **no existe ninguna app de voz Rust en producción que use `tonarino/webrtc-audio-processing` de forma pública y mantenida** (crates.io reverse deps vacío, GitHub search solo forks/experimentos). lumen-voice y echo-aec son los únicos consumidores no triviales — no hay "mejor práctica de la comunidad" que copiar; la referencia es C/C++ (PA/PW/pjproject) y el ADM de libwebrtc.
