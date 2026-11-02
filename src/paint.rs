//! Software rendering into a 0RGB u32 pixel buffer.

use std::collections::HashMap;
use std::sync::Arc;

use crate::font::Fonts;
use crate::image::Image;
use crate::layout::{ControlKind, Item, PageLayout, Target};

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub buf: Vec<u32>,
    /// [x0, y0, x1, y1)
    clip: [i32; 4],
    /// Images resampled to their display size, keyed by (image address, width, height).
    scaled: HashMap<(usize, u32, u32), Vec<u32>>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Self {
        Canvas { w, h, buf: vec![0xffffff; w * h], clip: [0, 0, w as i32, h as i32], scaled: HashMap::new() }
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        *self = Canvas::new(w, h);
    }

    pub fn set_clip(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.clip = [
            (x0 as i32).clamp(0, self.w as i32),
            (y0 as i32).clamp(0, self.h as i32),
            (x1 as i32).clamp(0, self.w as i32),
            (y1 as i32).clamp(0, self.h as i32),
        ];
    }

    pub fn clip(&self) -> [i32; 4] {
        self.clip
    }

    pub fn restore_clip(&mut self, clip: [i32; 4]) {
        self.clip = clip;
    }

    pub fn reset_clip(&mut self) {
        self.clip = [0, 0, self.w as i32, self.h as i32];
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: u32) {
        let x0 = (x.round() as i32).max(self.clip[0]);
        let y0 = (y.round() as i32).max(self.clip[1]);
        let x1 = ((x + w).round() as i32).min(self.clip[2]);
        let y1 = ((y + h).round() as i32).min(self.clip[3]);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for row in y0..y1 {
            let start = row as usize * self.w;
            self.buf[start + x0 as usize..start + x1 as usize].fill(color);
        }
    }

    /// Draws an image scaled to the box (x, y, w, h).
    pub fn image(&mut self, x: f32, y: f32, w: f32, h: f32, image: &Arc<Image>) {
        let (dw, dh) = ((w.round() as u32).max(1), (h.round() as u32).max(1));
        let (dx, dy) = (x.round() as i32, y.round() as i32);
        // Skip work for images that are entirely outside the clip
        if dx >= self.clip[2] || dy >= self.clip[3] || dx + dw as i32 <= self.clip[0] || dy + dh as i32 <= self.clip[1] {
            return;
        }
        // Pixel budget: a few huge images must not grow the cache without bound
        if self.scaled.len() > 64 || self.scaled.values().map(Vec::len).sum::<usize>() > 32 * 1024 * 1024 {
            self.scaled.clear();
        }
        let key = (Arc::as_ptr(image) as usize, dw, dh);
        let pixels = self.scaled.entry(key).or_insert_with(|| resample(image, dw, dh));
        for row in 0..dh as i32 {
            let y = dy + row;
            if y < self.clip[1] || y >= self.clip[3] {
                continue;
            }
            for col in 0..dw as i32 {
                let x = dx + col;
                if x < self.clip[0] || x >= self.clip[2] {
                    continue;
                }
                let px = pixels[(row as u32 * dw + col as u32) as usize];
                let a = (px >> 24) as u8;
                let i = y as usize * self.w + x as usize;
                match a {
                    0 => {}
                    255 => self.buf[i] = px & 0xffffff,
                    _ => self.buf[i] = blend(self.buf[i], px, a),
                }
            }
        }
    }

    /// Draws text and returns its width.
    pub fn text(&mut self, fonts: &mut Fonts, x: f32, baseline: f32, text: &str, size: f32, bold: bool, color: u32) -> f32 {
        let mut pen = x;
        for c in text.chars() {
            if pen > self.clip[2] as f32 {
                break;
            }
            let (m, bitmap) = fonts.glyph(c, size, bold);
            let gx = pen.round() as i32 + m.xmin;
            let gy = baseline.round() as i32 - m.height as i32 - m.ymin;
            self.blit(gx, gy, m.width, m.height, bitmap, color);
            pen += m.advance_width;
        }
        pen - x
    }

    fn blit(&mut self, gx: i32, gy: i32, gw: usize, gh: usize, coverage: &[u8], color: u32) {
        for row in 0..gh as i32 {
            let y = gy + row;
            if y < self.clip[1] || y >= self.clip[3] {
                continue;
            }
            for col in 0..gw as i32 {
                let x = gx + col;
                if x < self.clip[0] || x >= self.clip[2] {
                    continue;
                }
                let a = coverage[(row as usize) * gw + col as usize];
                if a == 0 {
                    continue;
                }
                let i = y as usize * self.w + x as usize;
                self.buf[i] = blend(self.buf[i], color, a);
            }
        }
    }
}

/// Resamples by averaging the source pixels under each target pixel (premultiplied), which
/// keeps downscaled photos smooth; upscaling degenerates to nearest-neighbour.
fn resample(image: &Image, dw: u32, dh: u32) -> Vec<u32> {
    let (sw, sh) = (image.stored_w as usize, image.stored_h as usize);
    let mut out = Vec::with_capacity((dw * dh) as usize);
    for oy in 0..dh as usize {
        let y0 = oy * sh / dh as usize;
        let y1 = (((oy + 1) * sh).div_ceil(dh as usize)).clamp(y0 + 1, sh);
        for ox in 0..dw as usize {
            let x0 = ox * sw / dw as usize;
            let x1 = (((ox + 1) * sw).div_ceil(dw as usize)).clamp(x0 + 1, sw);
            let (mut r, mut g, mut b, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for sy in y0..y1 {
                for &p in &image.pixels[sy * sw + x0..sy * sw + x1] {
                    let pa = (p >> 24) as u64;
                    r += ((p >> 16) & 0xff) as u64 * pa;
                    g += ((p >> 8) & 0xff) as u64 * pa;
                    b += (p & 0xff) as u64 * pa;
                    a += pa;
                    n += 1;
                }
            }
            out.push(if a == 0 {
                0
            } else {
                ((a / n) as u32) << 24 | ((r / a) as u32) << 16 | ((g / a) as u32) << 8 | (b / a) as u32
            });
        }
    }
    out
}

fn blend(dst: u32, src: u32, a: u8) -> u32 {
    let a = a as u32;
    let mix = |shift: u32| {
        let d = (dst >> shift) & 0xff;
        let s = (src >> shift) & 0xff;
        ((s * a + d * (255 - a)) / 255) << shift
    };
    mix(16) | mix(8) | mix(0)
}

/// Draws text fields with what the user typed (`values`, keyed by field id) and a caret in
/// the focused one. Restores the page clip (`top`..`bottom`) afterwards.
pub fn paint_fields(canvas: &mut Canvas, fonts: &mut Fonts, layout: &PageLayout, dy: f32, top: f32, bottom: f32, values: &HashMap<usize, String>, focused: Option<usize>) {
    let width = canvas.w as f32;
    for link in &layout.links {
        let (x, y, w, h) = (link.x, link.y + dy, link.w, link.h);
        if y > bottom || y + h < top {
            continue;
        }
        if let Target::Control(control) = &link.target {
            let state = values.get(&control.id).and_then(|v| v.parse::<usize>().ok()).unwrap_or(control.initial);
            match &control.kind {
                ControlKind::Checkbox | ControlKind::Radio => {
                    let radio = control.kind == ControlKind::Radio;
                    canvas.rect(x, y, w, h, 0x8f8f8f);
                    canvas.rect(x + 1.0, y + 1.0, w - 2.0, h - 2.0, 0xffffff);
                    if state == 1 {
                        let inset = if radio { w * 0.3 } else { w * 0.22 };
                        canvas.rect(x + inset, y + inset, w - 2.0 * inset, h - 2.0 * inset, 0x0b57d0);
                    }
                }
                ControlKind::Select(labels) => {
                    canvas.rect(x, y, w, h, 0x8f8f8f);
                    canvas.rect(x + 1.0, y + 1.0, w - 2.0, h - 2.0, 0xefefef);
                    let label = labels.get(state).map(String::as_str).unwrap_or("");
                    canvas.set_clip(x.max(0.0) + 2.0, y.max(top), (x + w - 18.0).min(width), (y + h).min(bottom));
                    canvas.text(fonts, x + 8.0, y + (h - control.size) / 2.0 + control.size * 0.85, label, control.size, false, control.color);
                    canvas.set_clip(0.0, top, width, bottom);
                    canvas.text(fonts, x + w - 16.0, y + (h - control.size) / 2.0 + control.size * 0.85, "▼", control.size * 0.7, false, 0x5f6368);
                }
            }
            continue;
        }
        let Target::Field(field) = &link.target else { continue };
        let is_focused = focused == Some(field.id);
        canvas.rect(x, y, w, h, if is_focused { 0x0b57d0 } else { 0x8f8f8f });
        canvas.rect(x + 1.0, y + 1.0, w - 2.0, h - 2.0, 0xffffff);

        let value = values.get(&field.id).unwrap_or(&field.initial);
        let (text, color) = match (value.is_empty(), field.secret) {
            (true, _) => (field.placeholder.clone(), 0x8a8a8a),
            (false, true) => ("•".repeat(value.chars().count()), field.color),
            (false, false) => (value.clone(), field.color),
        };
        let text_w = fonts.measure(&text, field.size, false);
        let room = w - 12.0;
        // Keep the end of the text (where the caret is) visible while typing
        let tx = if is_focused && text_w > room { x + 6.0 + room - text_w } else { x + 6.0 };
        let baseline = y + 4.0 + field.size * 1.1;
        canvas.set_clip(x.max(0.0) + 2.0, y.max(top), (x + w - 2.0).min(width), (y + h).min(bottom));
        canvas.text(fonts, tx, baseline, &text, field.size, false, color);
        if is_focused {
            let caret_x = if value.is_empty() { x + 6.0 } else { tx + text_w + 1.0 };
            canvas.rect(caret_x, y + 4.0, 1.5, field.size * 1.3, 0x0b57d0);
        }
        canvas.set_clip(0.0, top, width, bottom);
    }
}

/// Paints the page. `dy` is the vertical offset from page to screen coordinates.
pub fn paint_page(canvas: &mut Canvas, fonts: &mut Fonts, layout: &PageLayout, dy: f32, top: f32, bottom: f32) {
    let base = canvas.clip();
    let mut stack: Vec<[i32; 4]> = Vec::new();
    for item in &layout.items {
        match item {
            Item::PushClip { x, y, w, h } => {
                let cur = canvas.clip();
                stack.push(cur);
                let r = [x.round() as i32, (y + dy).round() as i32, (x + w).round() as i32, (y + dy + h).round() as i32];
                let c = [r[0].max(cur[0]), r[1].max(cur[1]), r[2].min(cur[2]), r[3].min(cur[3])];
                // An empty intersection must stay empty (x1 >= x0 checks reject it)
                canvas.restore_clip([c[0], c[1], c[2].max(c[0]), c[3].max(c[1])]);
            }
            Item::PopClip => {
                if let Some(prev) = stack.pop() {
                    canvas.restore_clip(prev);
                }
            }
            Item::Rect { x, y, w, h, color } => {
                let sy = y + dy;
                if sy < bottom && sy + h > top {
                    canvas.rect(*x, sy, *w, *h, *color);
                }
            }
            Item::Image { x, y, w, h, image } => {
                let sy = y + dy;
                if sy < bottom && sy + h > top {
                    canvas.image(*x, sy, *w, *h, image);
                }
            }
            Item::Text { x, y, text, size, bold, color } => {
                let sy = y + dy;
                if sy - size < bottom && sy + size * 0.4 > top {
                    canvas.text(fonts, *x, sy, text, *size, *bold, *color);
                }
            }
        }
    }
    canvas.restore_clip(base);
}
