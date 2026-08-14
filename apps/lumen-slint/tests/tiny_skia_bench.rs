//! Bench: cuánto tarda tiny-skia en hornear un frame del fuego con glow.
//! Mide el costo de GENERACIÓN (una vez al arrancar) — el runtime es el blit
//! de la Image, que ya medimos (~0.5% de un núcleo).
//!
//! Run: cargo test -p lumen-desktop --test tiny_skia_bench -- --nocapture

use tiny_skia::{BlendMode, Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Transform};

fn draw_glow_fire_frame() -> Pixmap {
    let w = 512u32;
    let h = 288u32;
    let mut pixmap = Pixmap::new(w, h).unwrap();

    // glow radial: círculo cálido semi-transparente detrás del fuego
    let glow = Color::from_rgba8(0xD1, 0x63, 0x5E, 60);
    let mut paint = Paint::default();
    paint.set_color_rgba8(0xD1, 0x63, 0x5E, 60);
    paint.blend_mode = BlendMode::SourceOver;
    let mut path = PathBuilder::new();
    path.push_circle(256.0, 200.0, 120.0);
    let path = path.finish().unwrap();
    pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);

    // "llama": triángulo pixel-art (placeholder — el sprite real lo reemplaza)
    let flame = Color::from_rgba8(0xFF, 0xA0, 0x30, 220);
    let mut fp = Paint::default();
    fp.set_color_rgba8(0xFF, 0xA0, 0x30, 220);
    let mut flame_path = PathBuilder::new();
    flame_path.move_to(256.0, 140.0);
    flame_path.line_to(300.0, 210.0);
    flame_path.line_to(212.0, 210.0);
    flame_path.close();
    pixmap.fill_path(&flame_path.finish().unwrap(), &fp, FillRule::Winding, Transform::identity(), None);

    pixmap
}

#[test]
fn bench_tiny_skia_fire_frame() {
    // warm-up
    let _ = draw_glow_fire_frame();

    let n = 200;
    let start = std::time::Instant::now();
    for _ in 0..n {
        let _ = draw_glow_fire_frame();
    }
    let elapsed = start.elapsed();
    let per = elapsed / n;
    println!("tiny-skia: {n} frames 512x288 en {elapsed:?} = {per:?} por frame ({:.2} ms)", per.as_secs_f64() * 1000.0);
    println!("=> 60 frames del ciclo: {:.2} ms total al arrancar (una sola vez)", per.as_secs_f64() * 1000.0 * 60.0);
    println!("=> vs runtime: el blit de la Image es ~0.5% de un nucleo (ya medido)");
    assert!(per.as_secs_f64() * 1000.0 * 60.0 < 500.0, "generacion demasiado lenta");
}
