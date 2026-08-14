//! Spike de validación: render GL propio sobre la escena de Slint.
//!
//! Prueba la vía de "hardware real" para la animación de la fogata:
//!   - `set_rendering_notifier` + `GraphicsAPI::NativeOpenGL` → dibujar en el
//!     contexto GL de Slint (AfterRendering = encima de la escena, antes del swap).
//!   - 20 quads animados cuyo movimiento se calcula en el VERTEX SHADER (GPU),
//!     el CPU solo actualiza uniforms por frame → coste CPU ≈ 0 por partícula.
//!   - Un `slint::Timer` pide `request_redraw()` a ~30 fps.
//!
//! Objetivo: validar que (1) el notifier se dispara por frame con request_redraw,
//! (2) el hilo main baja respecto al render del campfire por scene graph, y
//! (3) el repaint log / partial rendering se comporta como esperamos.
//!
//! Gated por `LUMEN_SPIKE_GL` — no afecta al build normal.

use std::ffi::CString;
use std::time::Instant;

use glow::HasContext;
use slint::{GraphicsAPI, RenderingState, Timer, TimerMode};

const NUM_QUADS: usize = 20;
const REDRAW_MS: u64 = 33; // ~30 fps
const WIN_W: i32 = 1100;
const WIN_H: i32 = 720;

const VERT_SRC: &str = r#"
#version 300 es
layout(location = 0) in vec2 aPos; // unidad quad -0.5..0.5
uniform vec2 uCenter;
uniform float uTime;
uniform float uPhase;
uniform float uSize;
uniform vec2 uResolution;
void main() {
    // movimiento en GPU: seno/coseno de uTime (el CPU solo pasa uTime + fase)
    vec2 drift = vec2(
        sin(uTime * 2.0 + uPhase) * 40.0,
        cos(uTime * 1.7 + uPhase) * 30.0
    );
    vec2 pos = aPos * uSize + uCenter + drift;
    // window → NDC
    vec2 ndc = vec2(pos.x / uResolution.x * 2.0 - 1.0,
                   1.0 - pos.y / uResolution.y * 2.0);
    gl_Position = vec4(ndc, 0.0, 1.0);
}
"#;

const FRAG_SRC: &str = r#"
#version 300 es
precision mediump float;
out vec4 FragColor;
uniform vec4 uColor;
void main() { FragColor = uColor; }
"#;

// Unidad quad: dos triángulos, 6 vértices vec2
const UNIT_QUAD: [f32; 12] = [
    -0.5, -0.5, 0.5, -0.5, 0.5, 0.5, // tri 1
    -0.5, -0.5, 0.5, 0.5, -0.5, 0.5, // tri 2
];

fn compile_shader(gl: &glow::Context, ty: u32, src: &str) -> glow::Shader {
    unsafe {
        let shader = gl.create_shader(ty).unwrap();
        gl.shader_source(shader, src);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            panic!("shader compile failed: {log}");
        }
        shader
    }
}

/// Crea y devuelve (program, vao, vbo). Se llama una vez en RenderingSetup.
fn create_pipeline(gl: &glow::Context) -> (glow::Program, glow::VertexArray, glow::Buffer) {
    unsafe {
        let vs = compile_shader(gl, glow::VERTEX_SHADER, VERT_SRC);
        let fs = compile_shader(gl, glow::FRAGMENT_SHADER, FRAG_SRC);
        let program = gl.create_program().unwrap();
        gl.attach_shader(program, vs);
        gl.attach_shader(program, fs);
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            panic!("program link failed: {}", gl.get_program_info_log(program));
        }
        gl.delete_shader(vs);
        gl.delete_shader(fs);

        let vao = gl.create_vertex_array().unwrap();
        gl.bind_vertex_array(Some(vao));
        let vbo = gl.create_buffer().unwrap();
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_u8_slice(
            glow::ARRAY_BUFFER,
            bytemuck_or_raw(&UNIT_QUAD),
            glow::STATIC_DRAW,
        );
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
        gl.enable_vertex_attrib_array(0);
        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        gl.bind_vertex_array(None);

        (program, vao, vbo)
    }
}

fn bytemuck_or_raw(v: &[f32]) -> &[u8] {
    // reinterpret f32 slice as bytes (safe: glow takes &[u8])
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4) }
}

/// Registra el notifier + el timer de redraw. Debe llamarse tras crear la ventana.
pub fn init(app: &(impl slint::ComponentHandle + 'static)) {
    let window = app.window();

    // ---- timer que fuerza redraw a ~30fps ----
    let weak = app.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, std::time::Duration::from_millis(REDRAW_MS), move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().request_redraw();
        }
    });
    // Leaked: vive toda la app (igual que el timer de tiles en voice.rs)
    std::mem::forget(timer);

    // ---- estado capturado por el notifier (FnMut, no Send: todo en hilo main) ----
    let mut gl: Option<glow::Context> = None;
    let mut program: Option<glow::Program> = None;
    let mut vao: Option<glow::VertexArray> = None;
    let mut _vbo: Option<glow::Buffer> = None;
    let mut u_time: Option<glow::UniformLocation> = None;
    let mut u_center: Option<glow::UniformLocation> = None;
    let mut u_phase: Option<glow::UniformLocation> = None;
    let mut u_size: Option<glow::UniformLocation> = None;
    let mut u_res: Option<glow::UniformLocation> = None;
    let mut u_color: Option<glow::UniformLocation> = None;
    let start = Instant::now();

    let res = window.set_rendering_notifier(move |state, api| {
        match state {
            RenderingState::RenderingSetup => {
                if let GraphicsAPI::NativeOpenGL { get_proc_address } = api {
                    unsafe {
                        let ctx = glow::Context::from_loader_function(|name| {
                            let c = CString::new(name).unwrap();
                            get_proc_address(&c)
                        });
                        let (prog, v, b) = create_pipeline(&ctx);
                        u_time = Some(ctx.get_uniform_location(prog, "uTime").unwrap());
                        u_center = Some(ctx.get_uniform_location(prog, "uCenter").unwrap());
                        u_phase = Some(ctx.get_uniform_location(prog, "uPhase").unwrap());
                        u_size = Some(ctx.get_uniform_location(prog, "uSize").unwrap());
                        u_res = Some(ctx.get_uniform_location(prog, "uResolution").unwrap());
                        u_color = Some(ctx.get_uniform_location(prog, "uColor").unwrap());
                        program = Some(prog);
                        vao = Some(v);
                        _vbo = Some(b);
                        gl = Some(ctx);
                    }
                }
            }
            RenderingState::BeforeRendering => {
                // (spike: dibujamos encima en AfterRendering para verlo)
            }
            RenderingState::AfterRendering => {
                if let (Some(gl), Some(prog), Some(vao)) = (gl.as_ref(), program, vao) {
                    unsafe {
                        // guardar estado que tocamos
                        let blend_enabled = gl.is_enabled(glow::BLEND);

                        gl.enable(glow::BLEND);
                        gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                        gl.use_program(Some(prog));
                        gl.bind_vertex_array(Some(vao));

                        let t = start.elapsed().as_secs_f32();
                        let (w, h) = (WIN_W as f32, WIN_H as f32);
                        gl.uniform_2_f32(u_res.as_ref(), w, h);

                        for i in 0..NUM_QUADS {
                            let phase = i as f32 * 1.7;
                            // centro repartido por la ventana
                            let cx = 80.0 + (i % 5) as f32 * 180.0;
                            let cy = 100.0 + (i / 5) as f32 * 120.0;
                            gl.uniform_2_f32(u_center.as_ref(), cx, cy);
                            gl.uniform_1_f32(u_time.as_ref(), t);
                            gl.uniform_1_f32(u_phase.as_ref(), phase);
                            gl.uniform_1_f32(u_size.as_ref(), 12.0 + (i % 3) as f32 * 4.0);
                            let a = 0.5 + 0.5 * (t + phase).sin();
                            gl.uniform_4_f32(
                                u_color.as_ref(),
                                0.3 + 0.6 * ((i as f32 * 0.13) % 1.0),
                                0.7,
                                0.9,
                                a,
                            );
                            gl.draw_arrays(glow::TRIANGLES, 0, 6);
                        }

                        // restaurar estado
                        gl.bind_vertex_array(None);
                        gl.use_program(None);
                        if !blend_enabled {
                            gl.disable(glow::BLEND);
                        }
                    }
                }
            }
            _ => {}
        }
    });
    match res {
        Ok(()) => eprintln!("[spike-gl] rendering notifier registrado OK"),
        Err(e) => eprintln!("[spike-gl] error al registrar notifier: {e:?}"),
    }
}
