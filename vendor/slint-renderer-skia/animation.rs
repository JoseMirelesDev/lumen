// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

/*!
Registry for pre-rendered animation frames — Lumen fork addition
(see docs/performance-fork-design.md).

The frames live OUTSIDE the reactive scene graph. The application registers them
once per key (`Window::register_animation_frames`) and switches the visible frame
with `Window::set_animation_frame`. Neither call touches a property binding; the
renderer repaints only the region that actually changed (the diff bounding box of
the old→new frame transition, precomputed once at registration, scaled into the
item's on-screen rectangle) — never the whole item, let alone the scene.

## Storage: sparse palette frames (RAM ~47× smaller than raw RGBA)

The campfire frames are pixel art: each 512×288 frame has ~2100 visible pixels of
147456 (~98.5% transparent) and only 11–22 distinct RGB colors (the art uses a
33-color GIMP palette). Storing raw RGBA (35 MB for 60 frames) is wasteful, so
`register` converts the straight frames into a **shared RGB palette** (exact-match,
≤256 entries — lossless for this art) plus a **per-frame sparse list of visible
pixels** `(x, y, palette_idx, alpha)` — 6 bytes each. Total: ~0.75 MB + 1 KB
palette + one reused sub-rect scratch (~0.6 MB) instead of ~35 MB.

If a sequence ever exceeds 255 distinct RGBs, the slot falls back to keeping the
premultiplied RGBA frames (the previous behavior) — correct, just not compact.

## Rendering

- **GPU** (canvas has a GPU context): each slot owns a persistent GPU `SkSurface`
  (one GL texture, width x height). Switching frames decodes only the diff
  sub-rect from the sparse list into a reused scratch (palette lookup +
  premultiply, ~1500 pixels — cheaper than the old full-sub-rect copy) and calls
  `write_pixels` — one `glTexSubImage2D` into the same texture. No per-tick CPU
  raster, no Skia texture-cache churn.
- **CPU** (software renderer, or GPU surface creation failed): the current frame
  is decoded from the sparse list into a full raster image on frame change.

NOTE: an earlier iteration stacked all frames in one tall atlas texture and
dropped the RAM buffers. That broke on GPUs whose max texture size is below
`height * frame_count` (Intel HD 4600: 16384 < 512*60 = 17280), silently falling
back to the CPU path — a ~35MB `raster_from_data` copy per frame. The single-frame
surface avoids the size limit entirely (measured: draw drops from ~0.5ms to ~10µs).
*/

use i_slint_core::graphics::{Rgba8Pixel, SharedPixelBuffer};
use i_slint_core::lengths::{LogicalPoint, LogicalRect, LogicalSize};
use i_slint_core::platform::PlatformError;
use std::cell::{Cell, Ref, RefCell};
use std::collections::HashMap;

/// A visible pixel in a frame: packed position, palette index, straight alpha.
/// The palette index is 0..255 (≤256 distinct RGBs in the sequence).
#[derive(Clone, Copy)]
struct SparsePx {
    x: u16,
    y: u16,
    idx: u8,
    a: u8,
}

/// Premultiply straight-alpha RGBA in place (fallback path only — the sparse
/// path premultiplies at decode time, per sub-rect, so no full-frame copy).
fn premultiply_in_place(buffer: &mut SharedPixelBuffer<Rgba8Pixel>) {
    for pixel in buffer.make_mut_slice() {
        let a = pixel.a as u16;
        pixel.r = ((pixel.r as u16) * a / 255) as u8;
        pixel.g = ((pixel.g as u16) * a / 255) as u8;
        pixel.b = ((pixel.b as u16) * a / 255) as u8;
    }
}

/// Per-slot frame storage.
///
/// Two storage modes, chosen at registration:
/// - **Sparse** (default): `palette` + `sparse` per-frame visible-pixel lists.
///   ~47× less RAM than raw RGBA for the pixel-art fire frames.
/// - **RGBA fallback**: `frames` keeps the premultiplied buffers when the
///   sequence has more than 255 distinct RGBs.
///
/// A single persistent GPU `SkSurface` (width x height) holds the CURRENT frame;
/// switching frames is one `write_pixels` of the diff sub-rect into that same
/// texture.
type DiffBBox = Option<(u32, u32, u32, u32)>;

fn diff_bbox(
    a: &SharedPixelBuffer<Rgba8Pixel>,
    b: &SharedPixelBuffer<Rgba8Pixel>,
) -> DiffBBox {
    debug_assert_eq!(a.width(), b.width());
    debug_assert_eq!(a.height(), b.height());
    let w = a.width() as usize;
    let h = a.height() as usize;
    let (sa, sb) = (a.as_bytes(), b.as_bytes());
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    for y in 0..h {
        let row = y * w * 4;
        for x in 0..w {
            let o = row + x * 4;
            if sa[o..o + 4] != sb[o..o + 4] {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    if max_x < min_x || max_y < min_y {
        None
    } else {
        Some((min_x as u32, min_y as u32, (max_x - min_x + 1) as u32, (max_y - min_y + 1) as u32))
    }
}

/// A registered frame sequence plus the renderer-side state needed to display it
/// without going through the scene graph.
pub struct AnimationSlot {
    width: u32,
    height: u32,
    /// Shared straight-RGB palette (index = the sparse pixels' `idx`). Built
    /// from the union of all frames' visible pixels; exact match (the pixel art
    /// has few colors), so the conversion is lossless.
    palette: Vec<[u8; 3]>,
    /// Per-frame sparse visible pixels (only where alpha > 0).
    sparse: RefCell<Option<Vec<Vec<SparsePx>>>>,
    /// Fallback: premultiplied RGBA frames, kept ONLY when the palette would
    /// exceed 255 entries. `None` in the sparse path.
    frames: RefCell<Option<Vec<SharedPixelBuffer<Rgba8Pixel>>>>,
    /// Bbox (frame coords) of the pixels that change when advancing from frame
    /// `i` to frame `i + 1`, used to repaint only the region that actually
    /// changed. `None` = identical frames.
    diff_bboxes: Vec<DiffBBox>,
    /// Index of the frame to display.
    current: Cell<usize>,
    /// Index of the frame currently uploaded to the GPU surface, if any.
    uploaded: Cell<Option<usize>>,
    /// The on-screen (logical) rectangle of the item at its last draw, so
    /// `set_animation_frame` knows which region to repaint.
    last_rect: RefCell<Option<LogicalRect>>,
    /// GPU surface holding the current frame (one GL texture, width x height)
    /// or CPU fallback, chosen at first draw.
    surface: RefCell<Option<skia_safe::Surface>>,
    /// Reused premultiplied-RGBA scratch for the per-tick `write_pixels`
    /// sub-rect decode (avoids a per-tick allocation).
    scratch: RefCell<Vec<u8>>,
}

impl AnimationSlot {
    fn new(
        palette: Vec<[u8; 3]>,
        sparse: Vec<Vec<SparsePx>>,
        frames: Option<Vec<SharedPixelBuffer<Rgba8Pixel>>>,
        diff_bboxes: Vec<DiffBBox>,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            width,
            height,
            palette,
            sparse: RefCell::new(Some(sparse)),
            frames: RefCell::new(frames),
            diff_bboxes,
            current: Cell::new(0),
            uploaded: Cell::new(None),
            last_rect: RefCell::new(None),
            surface: RefCell::new(None),
            scratch: RefCell::new(Vec::new()),
        }
    }

    /// Number of frames in the sequence.
    pub fn frame_count(&self) -> usize {
        if let Some(sparse) = self.sparse.borrow().as_ref() {
            sparse.len()
        } else {
            self.frames.borrow().as_ref().map_or(0, |f| f.len())
        }
    }

    /// Remember where the item was last drawn on screen, in logical coordinates.
    pub fn record_rect(&self, rect: LogicalRect) {
        *self.last_rect.borrow_mut() = Some(rect);
    }

    /// The on-screen rectangle of the item as of its last draw.
    pub fn last_rect(&self) -> Option<LogicalRect> {
        *self.last_rect.borrow()
    }

    fn frame_info(&self) -> skia_safe::ImageInfo {
        skia_safe::ImageInfo::new(
            skia_safe::ISize::new(self.width as i32, self.height as i32),
            skia_safe::ColorType::RGBA8888,
            skia_safe::AlphaType::Premul,
            None,
        )
    }

    /// Decode the sub-rect of `frame_idx` from the sparse list into the reused
    /// scratch as premultiplied RGBA. Pixels the frame doesn't cover (transparent)
    /// are left as (0,0,0,0) — replacing the surface region with transparent is
    /// exactly what removes the old frame's pixels on screen.
    fn decode_sparse_into_scratch(
        &self,
        frame_idx: usize,
        bx: u32,
        by: u32,
        bw: u32,
        bh: u32,
    ) -> (usize, u32) {
        let mut scratch = self.scratch.borrow_mut();
        let row_bytes = bw as usize * 4;
        let needed = bh as usize * row_bytes;
        if scratch.len() < needed {
            scratch.resize(needed, 0);
        }
        scratch[..needed].fill(0);

        let sparse = self.sparse.borrow();
        if let Some(Some(frame)) = sparse.as_ref().map(|s| s.get(frame_idx)) {
            let x0 = bx as i32;
            let y0 = by as i32;
            let x1 = (bx + bw) as i32;
            let y1 = (by + bh) as i32;
            for px in frame {
                let x = px.x as i32;
                let y = px.y as i32;
                if x >= x0 && x < x1 && y >= y0 && y < y1 {
                    let [r, g, b] = self.palette[px.idx as usize];
                    let a = px.a as u32;
                    let o = ((y - y0) as usize * row_bytes) + ((x - x0) as usize) * 4;
                    // Premultiply at decode (the palette is straight).
                    scratch[o] = (r as u32 * a / 255) as u8;
                    scratch[o + 1] = (g as u32 * a / 255) as u8;
                    scratch[o + 2] = (b as u32 * a / 255) as u8;
                    scratch[o + 3] = px.a;
                }
            }
        }
        drop(sparse);
        (row_bytes, needed as u32)
    }

    /// Upload the current frame into the GPU surface. On the first draw the
    /// surface is created (width x height — never taller than the frame, so no
    /// GPU max-texture-size limit); afterwards only the diff sub-rect of the
    /// old→new transition is written into the SAME texture (one
    /// `glTexSubImage2D`). Frame jumps (more than one step since the last
    /// upload, e.g. a skipped render) upload the full frame — the repaint
    /// region computed by `set_frame` covers the union of the skipped
    /// transitions in that case.
    fn upload_current(&self, surface: &mut skia_safe::Surface) {
        let current = self.current.get();
        let n = self.frame_count();
        if n == 0 {
            return;
        }
        let (bx, by, bw, bh) = match self.uploaded.get() {
            Some(u) if (u + 1) % n == current => match self.diff_bboxes[u] {
                Some(bbox) => bbox,
                // Identical transition: nothing changed on screen.
                None => {
                    self.uploaded.set(Some(current));
                    return;
                }
            },
            _ => (0, 0, self.width, self.height),
        };
        let bw = bw.min(self.width);
        let bh = bh.min(self.height);

        if self.sparse.borrow().is_some() {
            let (row_bytes, needed) = self.decode_sparse_into_scratch(current, bx, by, bw, bh);
            let mut scratch = self.scratch.borrow_mut();
            let bytes = &mut scratch[..needed as usize];
            let sub_info = skia_safe::ImageInfo::new(
                skia_safe::ISize::new(bw as i32, bh as i32),
                skia_safe::ColorType::RGBA8888,
                skia_safe::AlphaType::Premul,
                None,
            );
            if let Some(pixmap) = skia_safe::Pixmap::new(&sub_info, bytes, row_bytes) {
                surface.write_pixels_from_pixmap(&pixmap, (bx as i32, by as i32));
            }
            drop(scratch);
        } else if let Some(frames) = self.frames.borrow_mut().as_mut() {
            let Some(frame) = frames.get_mut(current) else { return };
            let row_bytes = self.width as usize * 4;
            let bytes = frame.make_mut_bytes();
            let offset = by as usize * row_bytes + bx as usize * 4;
            let sub = &mut bytes[offset..offset + bh as usize * row_bytes];
            let sub_info = skia_safe::ImageInfo::new(
                skia_safe::ISize::new(bw as i32, bh as i32),
                skia_safe::ColorType::RGBA8888,
                skia_safe::AlphaType::Premul,
                None,
            );
            if let Some(pixmap) = skia_safe::Pixmap::new(&sub_info, sub, row_bytes) {
                surface.write_pixels_from_pixmap(&pixmap, (bx as i32, by as i32));
            }
        } else {
            return;
        }
        self.uploaded.set(Some(current));
    }

    /// Returns the image to blit (the single-frame GPU surface) and its
    /// source-rect, or `None` if no GPU surface could be created (the caller
    /// falls back to a per-frame CPU raster in `draw_cpu`).
    pub fn update_surface(
        &self,
        canvas: &skia_safe::Canvas,
    ) -> Option<(skia_safe::Image, skia_safe::Rect)> {
        let n = self.frame_count();
        if n == 0 {
            return None;
        }
        let mut surface = self.surface.borrow_mut();
        if surface.is_none() {
            let mut new_surface = canvas.new_surface(&self.frame_info(), None);
            if let Some(s) = new_surface.as_mut() {
                self.uploaded.set(None);
                self.upload_current(s);
            }
            *surface = new_surface;
        }
        let surface = surface.as_mut()?;
        if self.uploaded.get() != Some(self.current.get()) {
            self.upload_current(surface);
        }
        let image = surface.image_snapshot();
        let src = skia_safe::Rect::from_xywh(
            0.,
            0.,
            self.width as f32,
            self.height as f32,
        );
        Some((image, src))
    }

    /// CPU fallback: build a raster image for the current frame (no GPU).
    pub fn draw_cpu(&self) -> Option<skia_safe::Image> {
        let info = self.frame_info();
        let current = self.current.get();
        if self.sparse.borrow().is_some() {
            // Decode the full frame from the sparse list.
            let (row_bytes, _) = self.decode_sparse_into_scratch(current, 0, 0, self.width, self.height);
            let scratch = self.scratch.borrow();
            let bytes = scratch[..self.height as usize * row_bytes].to_vec();
            drop(scratch);
            skia_safe::images::raster_from_data(&info, skia_safe::Data::new_copy(&bytes), row_bytes)
        } else {
            let frames = self.frames.borrow();
            let frame = frames.as_ref()?.get(current)?;
            skia_safe::images::raster_from_data(
                &info,
                skia_safe::Data::new_copy(frame.as_bytes()),
                self.width as usize * 4,
            )
        }
    }
}

/// Per-renderer collection of registered frame sequences.
#[derive(Default)]
pub struct AnimationRegistry {
    slots: RefCell<HashMap<u32, AnimationSlot>>,
}

impl AnimationRegistry {
    /// Register `frames` under `key`, replacing any previous registration.
    /// All frames must be non-empty and share the same size.
    ///
    /// The straight frames are converted to the sparse palette representation
    /// (lossless for ≤255 distinct RGBs — the pixel-art fire frames have ~100);
    /// sequences with more colors fall back to premultiplied RGBA in RAM.
    pub fn register(
        &self,
        key: u32,
        frames: Vec<SharedPixelBuffer<Rgba8Pixel>>,
    ) -> Result<(), PlatformError> {
        if frames.is_empty() {
            return Err("Animation frames must not be empty".into());
        }
        let (width, height) = (frames[0].width(), frames[0].height());
        if frames.iter().any(|f| f.width() != width || f.height() != height) {
            return Err("All animation frames must have the same size".into());
        }
        // Diff bboxes must be computed on the straight frames: premultiplying can
        // collide (different pixels mapping to the same premultiplied value) and
        // shrink the change region, causing stale pixels.
        let diff_bboxes = (0..frames.len())
            .map(|i| diff_bbox(&frames[i], &frames[(i + 1) % frames.len()]))
            .collect();

        // Build the shared palette from the union of visible RGBs (exact match).
        let mut palette: Vec<[u8; 3]> = Vec::new();
        let mut palette_index: HashMap<[u8; 3], u8> = HashMap::new();
        let mut sparse = Vec::with_capacity(frames.len());
        let mut overflow = false;
        for frame in &frames {
            let bytes = frame.as_bytes();
            let mut list = Vec::with_capacity(2200);
            for y in 0..height as usize {
                let row = y * width as usize * 4;
                for x in 0..width as usize {
                    let o = row + x * 4;
                    let a = bytes[o + 3];
                    if a == 0 {
                        continue;
                    }
                    let rgb = [bytes[o], bytes[o + 1], bytes[o + 2]];
                    let idx = match palette_index.get(&rgb) {
                        Some(&i) => i,
                        None => {
                            if palette.len() >= 256 {
                                overflow = true;
                                break;
                            }
                            let i = palette.len() as u8;
                            palette.push(rgb);
                            palette_index.insert(rgb, i);
                            i
                        }
                    };
                    list.push(SparsePx { x: x as u16, y: y as u16, idx, a });
                }
                if overflow {
                    break;
                }
            }
            if overflow {
                break;
            }
            sparse.push(list);
        }

        let slot = if overflow {
            // Too many colors: keep the premultiplied RGBA frames (previous path).
            let frames = frames.into_iter().map(|mut buffer| {
                premultiply_in_place(&mut buffer);
                buffer
            }).collect();
            AnimationSlot::new(
                Vec::new(),
                Vec::new(),
                Some(frames),
                diff_bboxes,
                width,
                height,
            )
        } else {
            AnimationSlot::new(
                palette,
                sparse,
                None,
                diff_bboxes,
                width,
                height,
            )
        };
        self.slots.borrow_mut().insert(key, slot);
        Ok(())
    }

    pub fn get(&self, key: u32) -> Option<Ref<'_, AnimationSlot>> {
        Ref::filter_map(self.slots.borrow(), |slots| slots.get(&key)).ok()
    }

    /// Switch the displayed frame for `key`. Returns the on-screen rectangle that
    /// actually changed (the diff of the old→new transition, scaled from frame
    /// coordinates into the item's rect), so the caller repaints only that region.
    /// Returns `None` if the key is unknown, the index is out of bounds, the item
    /// has never been drawn, or the two frames are pixel-identical (nothing changed).
    pub fn set_frame(&self, key: u32, frame_index: u32) -> Option<LogicalRect> {
        let slots = self.slots.borrow();
        let slot = slots.get(&key)?;
        let n = slot.frame_count();
        if n == 0 || frame_index as usize >= n {
            return None;
        }
        let old = slot.current.get();
        let new = frame_index as usize;
        slot.current.set(new);

        let item_rect = slot.last_rect()?;
        // Contract: the caller advances the frame by one (wrapping). The region
        // that changes on screen is the diff bbox of the old→new transition,
        // scaled from frame coords into the item's on-screen rect, so only that
        // area is repainted. Arbitrary jumps fall back to the full item rect.
        let (bx, by, bw, bh) = if (old + 1) % n == new {
            match slot.diff_bboxes[old] {
                Some(bbox) => bbox,
                // Frames identical: nothing changed on screen, no repaint needed.
                None => return None,
            }
        } else {
            (0, 0, slot.width, slot.height)
        };
        let frame_w = slot.width as f32;
        let frame_h = slot.height as f32;
        // Safety margin for sub-pixel sampling at the bbox edges.
        let inflate = 1.0;
        let mut rect = LogicalRect::new(
            LogicalPoint::new(
                item_rect.origin.x + bx as f32 / frame_w * item_rect.size.width - inflate,
                item_rect.origin.y + by as f32 / frame_h * item_rect.size.height - inflate,
            ),
            LogicalSize::new(
                bw as f32 / frame_w * item_rect.size.width + 2.0 * inflate,
                bh as f32 / frame_h * item_rect.size.height + 2.0 * inflate,
            ),
        );
        rect = rect.intersection(&item_rect).unwrap_or_default();
        Some(rect)
    }

    /// Drop the cached GPU surface (e.g. when the graphics context is lost or
    /// the surface is replaced, because GPU surfaces reference the old context).
    /// The sparse frames + palette are kept (they are the upload source), so on
    /// the next draw a fresh surface is created and the current frame
    /// re-uploaded. In practice this is called only on context loss.
    pub fn reset_storage(&self) {
        for slot in self.slots.borrow_mut().values() {
            *slot.surface.borrow_mut() = None;
            slot.uploaded.set(None);
        }
    }
}
