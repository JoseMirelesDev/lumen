use std::fs;
use std::path::Path;

use serde_json::Value;

// Slots de arte del artista (docs/art-pipeline.md). El manifiesto generado en
// ui/generated/art-manifest.slint resuelve cada slot de personaje al PNG real
// del artista (assets/avatars) y al placeholder en caso contrario; el
// ui/generated/scene-manifest.slint resuelve cada elemento de escena
// (designs/art/scenes/*.json) a su arte real (assets/scene) o a su
// placeholder (assets/pixel), porque @image-url es de tiempo de compilación:
// un archivo ausente rompería el build, y un archivo nuevo no se puede
// "descubrir" en runtime. Regenerar: cualquier cargo build (build.rs).
const SKINS: [&str; 7] = ["ivory", "gold", "crimson", "teal", "indigo", "stone", "bronze"];
const STATES: [&str; 3] = ["idle", "talking", "muted"];
const SCENES_DIR: &str = "../../designs/art/scenes";

fn exists(p: &str) -> bool {
    Path::new(p).exists()
}

/// Un skin tiene arte real cuando existen los 6 PNG (char+bust × 3 estados).
fn skin_has_real_art(skin: &str) -> bool {
    STATES.iter().all(|s| {
        exists(&format!("assets/avatars/char_{s}_{skin}.png"))
            && exists(&format!("assets/avatars/bust_{s}_{skin}.png"))
    })
}

fn emit_slot_resolution(out: &mut String, kind: &str, real_skins: &[(&str, bool)]) {
    out.push_str(&format!("\n    public pure function {kind}(skin: string, state: string) -> image {{\n"));
    for (skin, is_real) in real_skins {
        if *is_real {
            out.push_str(&format!("        if skin == \"{skin}\" {{\n"));
            for state in STATES {
                out.push_str(&format!(
                    "            if state == \"{state}\" {{ return @image-url(\"../../assets/avatars/{kind}_{state}_{skin}.png\"); }}\n"
                ));
            }
            out.push_str("        }\n");
        }
    }
    // fallback: placeholder SVG (siempre existe)
    for skin in SKINS {
        out.push_str(&format!("        if skin == \"{skin}\" {{\n"));
        for state in STATES {
            out.push_str(&format!(
                "            if state == \"{state}\" {{ return @image-url(\"../../assets/avatars/{kind}_{state}_{skin}.svg\"); }}\n"
            ));
        }
        out.push_str("        }\n");
    }
    out.push_str(&format!(
        "        return @image-url(\"../../assets/avatars/{kind}_idle_ivory.svg\");\n    }}\n"
    ));
}

fn generate_art_manifest() -> String {
    let mut out = String::new();
    out.push_str("// AUTO-GENERADO por apps/lumen-slint/build.rs — NO EDITAR.\n");
    out.push_str("// Resuelve cada slot de personaje al PNG real del artista (assets/avatars)\n");
    out.push_str("// o al placeholder cuando el arte aún no existe.\n");
    out.push_str("// El artista exporta con LibreSprite a designs/art/export y recompila.\n\n");
    out.push_str("export global ArtManifest {\n");

    let real_skins: Vec<(&str, bool)> = SKINS.iter().map(|s| (*s, skin_has_real_art(s))).collect();
    out.push_str("    // --- arte real presente (PNG del artista) por skin ---\n");
    for (skin, is_real) in &real_skins {
        out.push_str(&format!(
            "    out property <bool> real-{skin}: {};\n",
            if *is_real { "true" } else { "false" }
        ));
    }

    emit_slot_resolution(&mut out, "char", &real_skins);
    emit_slot_resolution(&mut out, "bust", &real_skins);

    out.push_str("}\n");
    out
}

// --- escena data-driven (designs/art/scenes/*.json) ------------------------

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

/// Longitud Slint: "64px" o "64.5px".
fn len(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}px", v as i64)
    } else {
        format!("{v:.2}px")
    }
}

/// Float Slint sin ceros residuales.
fn float(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

/// Resuelve los frames de un elemento: arte real (assets/scene/<id>_<n>.png)
/// si existe, si no el placeholder declarado en "fallback" (con {n} por
/// frame si el elemento tiene grilla). None = el elemento no se renderiza.
fn resolve_frames(el: &Value) -> Option<Vec<String>> {
    let id = el.get("id")?.as_str()?;
    let grid = el.get("grid");
    let frames = grid.and_then(|g| g.get("cols")).and_then(|c| c.as_u64()).unwrap_or(1) as usize;

    let real: Vec<String> = (0..frames)
        .map(|n| {
            if frames > 1 {
                format!("assets/scene/{id}_{n}.png")
            } else {
                format!("assets/scene/{id}.png")
            }
        })
        .collect();
    if real.iter().all(|p| exists(p)) {
        return Some(real);
    }

    if let Some(fb) = el.get("fallback").and_then(|f| f.as_str()) {
        if fb.contains("{n}") {
            let fbs: Vec<String> = (0..frames).map(|n| fb.replace("{n}", &n.to_string())).collect();
            if fbs.iter().all(|p| exists(&format!("assets/{p}"))) {
                return Some(fbs.iter().map(|p| format!("assets/{p}")).collect());
            }
        } else if exists(&format!("assets/{fb}")) {
            return Some(vec![format!("assets/{fb}")]);
        }
    }
    None
}

/// (x, y, w, h) en px de diseño — y desde arriba. Los elementos con "fit":
/// "fill" cubren el lienzo completo; el resto se ancla por bottom-center en
/// el punto (anchor.x, anchor.y) — y medido desde abajo.
fn element_geometry(el: &Value, dw: f64, dh: f64) -> (f64, f64, f64, f64) {
    if el.get("fit").and_then(|f| f.as_str()) == Some("fill") {
        return (0.0, 0.0, dw, dh);
    }
    let (w, h) = if let Some(g) = el.get("grid") {
        (num(&g["frameW"]), num(&g["frameH"]))
    } else if let Some(sz) = el.get("size").and_then(|s| s.as_array()) {
        (num(&sz[0]), num(&sz[1]))
    } else {
        (0.0, 0.0)
    };
    let ax = num(&el["anchor"]["x"]);
    let ay = num(&el["anchor"]["y"]);
    let x = ax * dw - w / 2.0;
    // anchor.y = fracción desde abajo donde apoya el borde inferior
    let y = ay * dh - h;
    (x, y, w, h)
}

fn emit_element(out: &mut String, el: &Value, dw: f64, dh: f64) {
    let id = el.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let Some(frames) = resolve_frames(el) else {
        out.push_str(&format!("        // {id}: sin arte real ni placeholder — omitido\n"));
        return;
    };
    let (x, y, w, h) = element_geometry(el, dw, dh);
    let anim_ms = el["anim"]["fps"].as_f64().map(|f| (1000.0 / f).round() as i64).unwrap_or(0);
    let flip = el.get("flip").and_then(|f| f.as_bool()).unwrap_or(false);
    let glow = el.get("glow").and_then(|g| g.as_str()).unwrap_or("#00000000");
    let glow = if glow.starts_with('#') { glow } else { "#00000000" };
    let min_w = el.get("min-w").as_ref().map(|m| len(num(m))).unwrap_or_else(|| "0px".to_string());

    let imgs: Vec<String> = frames.iter().map(|p| format!("@image-url(\"../../{p}\")")).collect();
    out.push_str(&format!(
        "        {{ imgs: [{}], x: {}, y: {}, w: {}, h: {}, anim-ms: {}, flip: {}, glow: {}, min-w: {} }}, // {id}\n",
        imgs.join(", "),
        len(x),
        len(y),
        len(w),
        len(h),
        anim_ms,
        if flip { "true" } else { "false" },
        glow,
        min_w
    ));
}

fn emit_particle(out: &mut String, p: &Value, dw: f64, dh: f64) {
    let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let Some(sprite) = p.get("sprite").and_then(|s| s.as_str()) else { return };
    let count = p.get("count").and_then(|c| c.as_u64()).unwrap_or(0) as usize;
    if count == 0 {
        return;
    }
    let real = format!("assets/scene/{sprite}");
    let path = if exists(&real) {
        real
    } else if let Some(fb) = p.get("fallback").and_then(|f| f.as_str()) {
        let fb = format!("assets/{fb}");
        if !exists(&fb) {
            out.push_str(&format!("        // {id}: sprite y placeholder ausentes — omitido\n"));
            return;
        }
        fb
    } else {
        out.push_str(&format!("        // {id}: sprite ausente — omitido\n"));
        return;
    };

    let spawn = &p["spawn"];
    let sx = num(&spawn["x"]) * dw;
    let sw = num(&spawn["w"]) * dw;
    let sy = (1.0 - num(&spawn["y"])) * dh;
    let sh = num(&spawn["h"]) * dh;
    let life = p.get("life").and_then(|l| l.as_array());
    let (life_min, life_max) = life
        .map(|a| (num(&a[0]), num(&a[1])))
        .unwrap_or((1.0, 2.0));
    let opacity = p.get("opacity").and_then(|o| o.as_array());
    let (op_start, op_end) = opacity
        .map(|a| (num(&a[0]), num(&a[1])))
        .unwrap_or((0.8, 0.0));

    let inds: Vec<String> = (0..count).map(|i| i.to_string()).collect();
    out.push_str(&format!(
        "        {{ img: @image-url(\"../../{path}\"), inds: [{}], spawn-x: {}, spawn-y: {}, spawn-w: {}, spawn-h: {}, rise: {}, drift: {}, sway: {}, sway-freq: {}, gravity: {}, life-min: {}, life-max: {}, size: {}, opacity-start: {}, opacity-end: {} }}, // {id}\n",
        inds.join(", "),
        len(sx),
        len(sy),
        len(sw),
        len(sh),
        len(num(&p["rise"])),
        len(num(&p["drift"])),
        len(num(&p["sway"])),
        float(num(&p["sway-freq"])),
        len(num(&p["gravity"])),
        float(life_min),
        float(life_max),
        len(num(&p["size"])),
        float(op_start),
        float(op_end)
    ));
}

fn generate_scene_manifest() -> String {
    let mut out = String::new();
    out.push_str("// AUTO-GENERADO por apps/lumen-slint/build.rs — NO EDITAR.\n");
    out.push_str("// Escena data-driven (designs/art/scenes/*.json): cada elemento se\n");
    out.push_str("// resuelve al PNG real del artista (assets/scene) o a su placeholder\n");
    out.push_str("// (assets/pixel). Añadir un elemento = entrada en el JSON + PNG en\n");
    out.push_str("// designs/art/export; el renderer (SceneProp/ParticleLayer) es genérico.\n\n");
    out.push_str("export struct SceneElement {\n");
    out.push_str("    imgs: [image],\n");
    out.push_str("    x: length,\n");
    out.push_str("    y: length,\n");
    out.push_str("    w: length,\n");
    out.push_str("    h: length,\n");
    out.push_str("    anim-ms: int,\n");
    out.push_str("    flip: bool,\n");
    out.push_str("    glow: color,\n");
    out.push_str("    min-w: length,\n");
    out.push_str("}\n\n");
    out.push_str("export struct ParticleDef {\n");
    out.push_str("    img: image,\n");
    out.push_str("    inds: [int],\n");
    out.push_str("    spawn-x: length,\n");
    out.push_str("    spawn-y: length,\n");
    out.push_str("    spawn-w: length,\n");
    out.push_str("    spawn-h: length,\n");
    out.push_str("    rise: length,\n");
    out.push_str("    drift: length,\n");
    out.push_str("    sway: length,\n");
    out.push_str("    sway-freq: float,\n");
    out.push_str("    gravity: length,\n");
    out.push_str("    life-min: float,\n");
    out.push_str("    life-max: float,\n");
    out.push_str("    size: length,\n");
    out.push_str("    opacity-start: float,\n");
    out.push_str("    opacity-end: float,\n");
    out.push_str("}\n\n");
    out.push_str("export global SceneManifest {\n");

    // dimensiones de diseño: de la primera escena con "design"
    let mut dw = 1024.0;
    let mut dh = 576.0;
    let mut scene_files: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(SCENES_DIR) {
        for e in entries.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("json") {
                if let Some(name) = e.path().file_stem().and_then(|s| s.to_str()) {
                    scene_files.push(name.to_string());
                }
            }
        }
    } else {
        panic!("build.rs: no existe designs/art/scenes/ — el arte es data-driven (docs/art-pipeline.md)");
    }
    scene_files.sort();

    for name in &scene_files {
        let path = format!("{SCENES_DIR}/{name}.json");
        let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("build.rs: leer {path}: {e}"));
        let scene: Value =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("build.rs: parsear {path}: {e}"));
        if let Some(d) = scene.get("design") {
            dw = num(&d["w"]);
            dh = num(&d["h"]);
        }

        out.push_str(&format!(
            "    out property <length> design-w: {};\n",
            len(dw)
        ));
        out.push_str(&format!(
            "    out property <length> design-h: {};\n",
            len(dh)
        ));

        out.push_str(&format!(
            "\n    // --- escena {name}: elementos (orden = z, painters' algorithm) ---\n"
        ));
        out.push_str(&format!("    out property <[SceneElement]> {name}: [\n"));
        if let Some(elements) = scene.get("elements").and_then(|e| e.as_array()) {
            for el in elements {
                emit_element(&mut out, el, dw, dh);
            }
        }
        out.push_str("    ];\n");

        out.push_str(&format!(
            "\n    // --- escena {name}: partículas (emisores data-driven) ---\n"
        ));
        out.push_str(&format!("    out property <[ParticleDef]> {name}-particles: [\n"));
        if let Some(particles) = scene.get("particles").and_then(|p| p.as_array()) {
            for p in particles {
                emit_particle(&mut out, p, dw, dh);
            }
        }
        out.push_str("    ];\n");
    }

    out.push_str("}\n");
    out
}

fn main() {
    let art_manifest = generate_art_manifest();
    let scene_manifest = generate_scene_manifest();
    let gen_dir = Path::new("ui/generated");
    fs::create_dir_all(gen_dir).expect("crear ui/generated");
    fs::write(gen_dir.join("art-manifest.slint"), art_manifest).expect("escribir art-manifest.slint");
    fs::write(gen_dir.join("scene-manifest.slint"), scene_manifest)
        .expect("escribir scene-manifest.slint");

    // Embed @font-face fonts AND @image-url images as-is into the binary
    // (decoded at run-time). No runtime file-path dependencies: fonts and
    // pixel assets ship inside the executable.
    let mut config = slint_build::CompilerConfiguration::new();
    config = config.embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    // The embedded MCP server needs element ids/source locations to target
    // elements by id. Only enabled under the `mcp` feature (a debug-only
    // build mode); normal builds skip it.
    if std::env::var("CARGO_FEATURE_MCP").is_ok() {
        config = config.with_debug_info(true);
    }
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint build failed");
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=assets");
    println!("cargo:rerun-if-changed=../../designs/art/scenes");
    println!("cargo:rerun-if-changed=../../designs/art/export");
}
