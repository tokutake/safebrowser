//! System font loading and glyph caching (rasterized by fontdue, pure Rust).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use fontdue::{Font, FontSettings, Metrics};

const MAX_CACHED_GLYPHS: usize = 20_000;
const MAX_CACHED_ADVANCES: usize = 100_000;
const FALLBACK_FONT: &str = "/System/Library/Fonts/Supplemental/Arial Unicode.ttf";
/// Used when the macOS fonts are missing (Linux): (regular, bold).
const LINUX_FONTS: [(&str, &str); 2] = [
    ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"),
    ("/usr/share/fonts/TTF/DejaVuSans.ttf", "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf"),
];

type GlyphKey = (char, u16, bool);

pub struct Fonts {
    regular: Font,
    bold: Font,
    advances: HashMap<GlyphKey, f32>,
    glyphs: HashMap<GlyphKey, (Metrics, Vec<u8>)>,
}

impl Fonts {
    /// Loads Hiragino Sans (W3 / W6), which covers Japanese.
    pub fn load() -> Result<Fonts> {
        let linux = LINUX_FONTS.iter().find(|(regular, _)| Path::new(regular).exists());
        let regular_path = find_system_font("W3.ttc")
            .or_else(|| linux.map(|(regular, _)| PathBuf::from(regular)))
            .unwrap_or_else(|| PathBuf::from(FALLBACK_FONT));
        let bold_path = find_system_font("W6.ttc")
            .or_else(|| linux.map(|(_, bold)| PathBuf::from(bold)))
            .unwrap_or_else(|| regular_path.clone());
        Ok(Fonts {
            regular: load_font(&regular_path)?,
            bold: load_font(&bold_path)?,
            advances: HashMap::new(),
            glyphs: HashMap::new(),
        })
    }

    pub fn measure(&mut self, text: &str, size: f32, bold: bool) -> f32 {
        let k = size_key(size);
        text.chars().map(|c| self.advance(normalize(c), k, bold)).sum()
    }

    fn advance(&mut self, c: char, k: u16, bold: bool) -> f32 {
        if let Some(a) = self.advances.get(&(c, k, bold)) {
            return *a;
        }
        let font = if bold { &self.bold } else { &self.regular };
        let a = font.metrics(c, k as f32 / 2.0).advance_width;
        if self.advances.len() >= MAX_CACHED_ADVANCES {
            self.advances.clear();
        }
        self.advances.insert((c, k, bold), a);
        a
    }

    pub fn glyph(&mut self, c: char, size: f32, bold: bool) -> &(Metrics, Vec<u8>) {
        let key = (normalize(c), size_key(size), bold);
        if self.glyphs.len() >= MAX_CACHED_GLYPHS && !self.glyphs.contains_key(&key) {
            self.glyphs.clear();
        }
        let font = if bold { &self.bold } else { &self.regular };
        self.glyphs.entry(key).or_insert_with(|| font.rasterize(key.0, key.1 as f32 / 2.0))
    }
}

/// Rounds to 0.5px steps for use as a cache key.
fn size_key(size: f32) -> u16 {
    (size * 2.0).round().clamp(8.0, 400.0) as u16
}

fn normalize(c: char) -> char {
    if c == '\u{a0}' { ' ' } else { c }
}

fn find_system_font(suffix: &str) -> Option<PathBuf> {
    std::fs::read_dir("/System/Library/Fonts")
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(suffix)))
}

fn load_font(path: &Path) -> Result<Font> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read font: {}", path.display()))?;
    Font::from_bytes(bytes, FontSettings { collection_index: 0, scale: 40.0, ..FontSettings::default() })
        .map_err(|e| anyhow!("cannot parse font ({}): {e}", path.display()))
}

