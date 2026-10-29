//! Software rendering into a 0RGB u32 pixel buffer.

use std::collections::HashMap;

use crate::font::Fonts;
use crate::layout::{Item, PageLayout, Target};

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub buf: Vec<u32>,
    /// [x0, y0, x1, y1)
    clip: [i32; 4],
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Self {
        Canvas { w, h, buf: vec![0xffffff; w * h], clip: [0, 0, w as i32, h as i32] }
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
        let Target::Field(field) = &link.target else { continue };
        let (x, y, w, h) = (link.x, link.y + dy, link.w, link.h);
        if y > bottom || y + h < top {
            continue;
        }
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
    for item in &layout.items {
        match item {
            Item::Rect { x, y, w, h, color } => {
                let sy = y + dy;
                if sy < bottom && sy + h > top {
                    canvas.rect(*x, sy, *w, *h, *color);
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
}
