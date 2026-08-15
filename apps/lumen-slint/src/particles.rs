//! Generador de frames del campfire en RAM (fuego + partículas).
//!
//! Todo lo que ANIMA (brazier, torches, brasas, luciérnagas) se pre-renderiza
//! a frames RGBA en memoria y se muestra como UN solo `AnimationImage` cuyo
//! frame rota cada tick desde Rust — FUERA del árbol reactivo de Slint. Cambiar
//! de frame NO toca bindings ni re-renderiza la escena: el renderer re-uploada
//! el frame a la textura GPU del item (glTexSubImage2D) y repinta solo su rect
//! (mark_dirty_region). El árbol del voice view queda con fondo cacheado +
//! 1 AnimationImage + seats + HUD (medido: render UI ~0.5% de un núcleo, idle).
//!
//! Matemática portada de `designs/art/scenes/campfire.json` + build.rs:
//!   - partículas: spawn-x = x*1024, spawn-y = (1-y)*576, px/py/palpha/psize
//!   - fuego: element_geometry (anchor), frame = floor(time*1000/anim-ms) % n


use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};

/// Key con el que se registran los frames del campfire. Debe coincidir con el
/// `animation-key` del `AnimationImage` en voice-view.slint.
const ANIMATION_KEY: u32 = 1;

use crate::AppWindow;

// ---------------------------------------------------------------------------
// Parámetros de diseño (portados de build.rs + campfire.json)
// ---------------------------------------------------------------------------
const DESIGN_W: f32 = 1024.0;
const DESIGN_H: f32 = 576.0;

// frame RGBA del área de la escena (resolución de render en RAM)
const FRAME_W: usize = 512;
const FRAME_H: usize = 288;
// escala diseño (1024x576) -> frame (512x288) = 0.5
const FRAME_SCALE: f32 = 0.5;

/// Paso de generación de frames: la animación se hornea a 80 ms por frame.
/// El playback avanza vía `tick_fire` (cabalga el push del voice, ~10 Hz) —
/// ligeramente más lento que el horneado, sin renders propios.
const STEP_MS: u64 = 80;
/// Frames del loop: 60 = 4.8 s de ciclo horneado.
const N_FRAMES: usize = 60;

// ---------------------------------------------------------------------------
// Defs
// ---------------------------------------------------------------------------
struct EmitterDef {
    count: usize,
    spawn_x: f32,
    spawn_y: f32,
    spawn_w: f32,
    spawn_h: f32,
    rise: f32,
    drift: f32,
    sway: f32,
    sway_freq: f32,
    gravity: f32,
    life_min: f32,
    life_max: f32,
    size: f32,
    opacity_start: f32,
    opacity_end: f32,
    sprite: Vec<u8>,
    sprite_w: u32,
    sprite_h: u32,
}

/// Un elemento de escena animado (fuego): sprite(s) de frame + geometría.
struct FireDef {
    // un sprite por frame del ciclo (campfire_0..3, torch_0..1)
    sprites: Vec<Sprite>,
    /// geometría en px de diseño (x, y, w, h) — y desde arriba
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    /// ms por frame (1000 / fps)
    anim_ms: f32,
    flip: bool,
}

struct Sprite {
    data: Vec<u8>,
    w: u32,
    h: u32,
}

// ---------------------------------------------------------------------------
// Partículas: matemática del .slint
// ---------------------------------------------------------------------------
fn hash(i: usize) -> f32 {
    // Math.mod(Math.abs(Math.sin(i * 12.9898 * 1deg) * 43758.5453), 1.0)
    let rad = (i as f32) * 12.9898_f32.to_radians();
    let v = rad.sin() * 43758.5453;
    v.abs().fract()
}

fn life_of(d: &EmitterDef, i: usize) -> f32 {
    d.life_min + hash(i * 7 + 3) * (d.life_max - d.life_min)
}

fn age_of(d: &EmitterDef, i: usize, time: f32) -> f32 {
    time % life_of(d, i)
}

fn px_of(d: &EmitterDef, i: usize, time: f32) -> f32 {
    let a = age_of(d, i, time);
    let x0 = d.spawn_x + d.spawn_w * hash(i * 13 + 1);
    let wob = d.sway * ((d.sway_freq * a * 57.2958 + hash(i * 5 + 2) * 360.0).to_radians()).sin();
    x0 + d.drift * a + wob
}

fn py_of(d: &EmitterDef, i: usize, time: f32) -> f32 {
    let a = age_of(d, i, time);
    let y0 = d.spawn_y + d.spawn_h * hash(i * 17 + 5);
    let v = d.rise * a + 0.5 * d.gravity * a * a;
    y0 - v
}

fn palpha_of(d: &EmitterDef, i: usize, time: f32) -> f32 {
    let a = age_of(d, i, time);
    let l = life_of(d, i);
    d.opacity_start + (d.opacity_end - d.opacity_start) * (a / l)
}

fn psize_of(d: &EmitterDef, i: usize) -> f32 {
    d.size * (0.7 + 0.6 * hash(i * 3 + 1))
}

// ---------------------------------------------------------------------------
// Blit
// ---------------------------------------------------------------------------

/// Dibuja un sprite escalado a un rect destino (con flip y alpha), blending
/// source-over. Reutilizado por partículas (cuadrados centrados) y fuego.
fn blit_sprite(
    buf: &mut SharedPixelBuffer<Rgba8Pixel>,
    sprite: &Sprite,
    dx: f32,
    dy: f32,
    dw: f32,
    dh: f32,
    alpha: f32,
    flip: bool,
) {
    let w = FRAME_W as u32;
    let h = FRAME_H as u32;
    if alpha <= 0.0 || dw <= 0.0 || dh <= 0.0 {
        return;
    }
    let pixels = buf.make_mut_slice();
    let sw = sprite.w as usize;
    let sh = sprite.h as usize;
    let dwpx = dw.ceil() as i32;
    let dhpx = dh.ceil() as i32;
    let dx0 = dx as i32;
    let dy0 = dy as i32;
    for sy in 0..dhpx {
        let ty = dy0 + sy;
        if ty < 0 || ty >= h as i32 {
            continue;
        }
        let src_y = ((sy as f32 / dhpx as f32) * sh as f32).floor() as usize % sh as usize;
        for sx in 0..dwpx {
            let tx = dx0 + sx;
            if tx < 0 || tx >= w as i32 {
                continue;
            }
            let fx = if flip { dwpx - 1 - sx } else { sx };
            let src_x = ((fx as f32 / dwpx as f32) * sw as f32).floor() as usize % sw as usize;
            let si = (src_y * sw + src_x) * 4;
            let sa = sprite.data[si + 3] as f32 / 255.0;
            let a = sa * alpha;
            if a <= 0.0 {
                continue;
            }
            let di = (ty as usize * w as usize + tx as usize);
            let mut dst = pixels[di];
            let oa = dst.a as f32 / 255.0;
            let out_a = a + oa * (1.0 - a);
            if out_a <= 0.0 {
                continue;
            }
            let (sr, sg, sb) = (
                sprite.data[si] as f32 / 255.0,
                sprite.data[si + 1] as f32 / 255.0,
                sprite.data[si + 2] as f32 / 255.0,
            );
            let (dr, dg, db) = (dst.r as f32 / 255.0, dst.g as f32 / 255.0, dst.b as f32 / 255.0);
            dst.r = ((sr * a + dr * oa * (1.0 - a)) / out_a * 255.0) as u8;
            dst.g = ((sg * a + dg * oa * (1.0 - a)) / out_a * 255.0) as u8;
            dst.b = ((sb * a + db * oa * (1.0 - a)) / out_a * 255.0) as u8;
            dst.a = (out_a * 255.0) as u8;
            pixels[di] = dst;
        }
    }
}

/// Dibuja un emisor de partículas: cada partícula es un quad centrado en
/// (px, py) con el sprite escalado al tamaño `psize`.
fn draw_emitter(buf: &mut SharedPixelBuffer<Rgba8Pixel>, d: &EmitterDef, time: f32) {
    let sprite = Sprite { data: d.sprite.clone(), w: d.sprite_w, h: d.sprite_h };
    for i in 0..d.count {
        let px = px_of(d, i, time) * FRAME_SCALE;
        let py = py_of(d, i, time) * FRAME_SCALE;
        let alpha = palpha_of(d, i, time).clamp(0.0, 1.0);
        let size = psize_of(d, i) * FRAME_SCALE;
        let half = size / 2.0;
        blit_sprite(buf, &sprite, px - half, py - half, size, size, alpha, false);
    }
}

/// Dibuja un elemento de fuego: selecciona el frame por el tiempo (reloj lento).
fn draw_fire(buf: &mut SharedPixelBuffer<Rgba8Pixel>, f: &FireDef, time: f32) {
    let frame = ((time * 1000.0 / f.anim_ms) as usize) % f.sprites.len();
    let sprite = &f.sprites[frame];
    // geometría diseño -> frame
    let dx = f.x * FRAME_SCALE;
    let dy = f.y * FRAME_SCALE;
    let dw = f.w * FRAME_SCALE;
    let dh = f.h * FRAME_SCALE;
    blit_sprite(buf, sprite, dx, dy, dw, dh, 1.0, f.flip);
}

/// Genera todos los frames del loop en RAM: fondo del fuego + partículas.
fn generate_frames(
    embers: &EmitterDef,
    fireflies: &EmitterDef,
    brazier: &FireDef,
    torch_left: &FireDef,
    torch_right: &FireDef,
) -> Vec<SharedPixelBuffer<Rgba8Pixel>> {
    let mut frames = Vec::with_capacity(N_FRAMES);
    let step_sec = STEP_MS as f32 / 1000.0;

    for f in 0..N_FRAMES {
        let time = f as f32 * step_sec;
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(FRAME_W as u32, FRAME_H as u32);
        // fuego (reloj lento: 5 fps brazier, 4 fps torches)
        draw_fire(&mut buf, brazier, time);
        draw_fire(&mut buf, torch_left, time);
        draw_fire(&mut buf, torch_right, time);
        // partículas (reloj rápido)
        draw_emitter(&mut buf, embers, time);
        draw_emitter(&mut buf, fireflies, time);
        frames.push(buf);
    }
    frames
}

// ---------------------------------------------------------------------------
// Carga de sprites — EMBEBIDOS en build time (build.rs → OUT_DIR/sprites.rs).
// El binario NO depende del CWD: funciona desde cualquier directorio.
// ---------------------------------------------------------------------------
include!(concat!(env!("OUT_DIR"), "/sprites.rs"));

fn load_sprite(name: &str) -> Sprite {
    let sd = sprite(name).unwrap_or_else(|| panic!("particles: sprite embebido faltante: {name}"));
    Sprite { data: sd.data.to_vec(), w: sd.w, h: sd.h }
}

fn load_fire_frames(base: &str, n: usize) -> Vec<Sprite> {
    (0..n).map(|i| load_sprite(&format!("{base}_{i}"))).collect()
}

fn emitter_from(name: &str, count: usize, sx: f32, sy: f32, sw: f32, sh: f32, rise: f32, drift: f32, sway: f32, sway_freq: f32, gravity: f32, life_min: f32, life_max: f32, size: f32, op_start: f32, op_end: f32) -> EmitterDef {
    let sprite = load_sprite(name);
    EmitterDef {
        count, spawn_x: sx, spawn_y: sy, spawn_w: sw, spawn_h: sh,
        rise, drift, sway, sway_freq, gravity,
        life_min, life_max, size, opacity_start: op_start, opacity_end: op_end,
        sprite: sprite.data, sprite_w: sprite.w, sprite_h: sprite.h,
    }
}

/// Inicializa: genera los frames en RAM (fuego + partículas) y los registra en
/// el renderer (textura GPU). Debe llamarse una vez (tras crear la ventana).
pub fn init(app: &AppWindow) {
    // embers: spawn=(451,426) size=(123x35) rise=58 grav=-30 size=9 life=[1.4,3.0] op=[0.9,0.0]
    let embers = emitter_from("ember", 22, 451.0, 426.0, 123.0, 35.0, 58.0, 5.0, 26.0, 2.4, -30.0, 1.4, 3.0, 9.0, 0.9, 0.0);
    // fireflies: spawn=(61,374) size=(901x288) rise=-4 grav=0 size=7 life=[3.0,6.5] op=[0.65,0.0]
    let fireflies = emitter_from("firefly", 12, 61.0, 374.0, 901.0, 288.0, -4.0, 7.0, 46.0, 0.85, 0.0, 3.0, 6.5, 7.0, 0.65, 0.0);

    // fuego: brazier anchor (0.5,1.0) grid 256x192 5fps = x 384 y 384; 4 frames
    let brazier = FireDef {
        sprites: load_fire_frames("campfire", 4),
        x: 0.5 * DESIGN_W - 256.0 / 2.0, // 384
        y: 1.0 * DESIGN_H - 192.0,       // 384
        w: 256.0,
        h: 192.0,
        anim_ms: 1000.0 / 5.0, // 200 ms
        flip: false,
    };
    // torches: anchor (0.06,0.9) y (0.94,0.9), grid 48x96 4fps
    let torch_left = FireDef {
        sprites: load_fire_frames("torch", 2),
        x: 0.06 * DESIGN_W - 48.0 / 2.0, // 37.44
        y: 0.9 * DESIGN_H - 96.0,        // 422.4
        w: 48.0,
        h: 96.0,
        anim_ms: 1000.0 / 4.0, // 250 ms
        flip: false,
    };
    let torch_right = FireDef {
        sprites: load_fire_frames("torch", 2),
        x: 0.94 * DESIGN_W - 48.0 / 2.0, // 938.56
        y: 0.9 * DESIGN_H - 96.0,        // 422.4
        w: 48.0,
        h: 96.0,
        anim_ms: 1000.0 / 4.0,
        flip: true,
    };

    let frames = generate_frames(&embers, &fireflies, &brazier, &torch_left, &torch_right);
    eprintln!("[particles] {} frames de {}x{} en RAM (fuego+partículas), {:.1} MB",
        frames.len(), FRAME_W, FRAME_H, frames.len() * FRAME_W * FRAME_H * 4 / 1_000_000);

    // Sube los frames a la textura GPU del item UNA vez. De aquí en más, cambiar
    // de frame es un glTexSubImage2D (sub-rect) + mark_dirty_region(bbox) — sin
    // bindings, sin re-render del resto de la escena.
    app.window()
        .register_animation_frames(ANIMATION_KEY, frames)
        .expect("particles: no se pudieron registrar los frames del campfire");

    // El renderer convierte los frames RGBA (35 MB) a su representación dispersa
    // (paleta + píxeles visibles, ~0.8 MB) y libera los buffers. glibc retiene la
    // memoria liberada en su arena — devolverla al OS para que el RSS refleje el
    // ahorro real.
    #[cfg(target_os = "linux")]
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        malloc_trim(0);
    }
}

/// Avanza un frame del campfire. Se llama desde el push del voice controller
/// (~12.5 Hz, el level stream a 80 ms): el mark_dirty_region + request_redraw
/// coalescen con el render que ya dispara el level stream → el fuego corre a su
/// cadencia horneada (12.5 Hz) sin agregar renders propios.
/// No-op si el key no está registrado (LUMEN_PARTICLES apagado) o si el item
/// aún no se dibujó.
pub fn tick_fire(window: &slint::Window) {
    static IDX: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let i = IDX.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 60;
    window.set_animation_frame(ANIMATION_KEY, i);
}
