//! Image decoding (PNG, JPEG, GIF, WebP, BMP) with the `image` crate, which is pure Rust.
//!
//! Hostile images are bounded: the encoded size is capped by the caller, the decoder gets
//! dimension and allocation limits, and big images are downscaled so a page of large photos
//! cannot exhaust memory.

use std::io::Cursor;

use image::{DynamicImage, ImageReader, Limits, imageops::FilterType};

/// Largest width or height the decoder will accept.
const MAX_DIMENSION: u32 = 8192;
/// Decoder allocation budget.
const MAX_ALLOC: u64 = 256 * 1024 * 1024;
/// Images with a longer side than this are stored downscaled.
const MAX_STORED_SIDE: u32 = 2048;

pub struct Image {
    /// Size the page sees (before any downscaling for storage).
    pub width: u32,
    pub height: u32,
    /// Size of `pixels`.
    pub stored_w: u32,
    pub stored_h: u32,
    /// 0xAARRGGBB, not premultiplied.
    pub pixels: Vec<u32>,
}

pub fn decode(bytes: &[u8]) -> Option<Image> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    let (width, height) = (decoded.width(), decoded.height());
    if width == 0 || height == 0 {
        return None;
    }
    let decoded = if width.max(height) > MAX_STORED_SIDE {
        let scale = MAX_STORED_SIDE as f32 / width.max(height) as f32;
        let nw = ((width as f32 * scale).round() as u32).max(1);
        let nh = ((height as f32 * scale).round() as u32).max(1);
        DynamicImage::ImageRgba8(decoded.resize_exact(nw, nh, FilterType::Triangle).to_rgba8())
    } else {
        decoded
    };
    let rgba = decoded.to_rgba8();
    let (stored_w, stored_h) = (rgba.width(), rgba.height());
    let pixels = rgba
        .pixels()
        .map(|p| (p[3] as u32) << 24 | (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32)
        .collect();
    Some(Image { width, height, stored_w, stored_h, pixels })
}

/// Decodes the payload of a `data:image/...;base64,` URL.
pub fn decode_data_url(url: &str) -> Option<Image> {
    let rest = url.strip_prefix("data:image/")?;
    let (meta, data) = rest.split_once(',')?;
    // SVG is a document format, not a raster one
    if meta.starts_with("svg") || !meta.ends_with(";base64") {
        return None;
    }
    decode(&base64_decode(data)?)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1x1 red PNG
    const RED_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    #[test]
    fn decodes_a_data_url() {
        let img = decode_data_url(&format!("data:image/png;base64,{RED_PNG}")).expect("decodes");
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.pixels[0], 0xffff0000);
    }

    #[test]
    fn rejects_garbage_and_svg() {
        assert!(decode(b"not an image").is_none());
        assert!(decode_data_url("data:image/svg+xml;base64,PHN2Zz48L3N2Zz4=").is_none());
        assert!(decode_data_url("data:image/png;base64,@@@").is_none());
    }
}
