//! Font store: loads Windows system TTFs, resolves per-glyph fallback
//! (Segoe UI -> Malgun Gothic for Hangul -> Segoe UI Symbol), measures
//! text with kerning, and caches rasterized glyphs.

use std::collections::HashMap;
use std::fs;

pub struct Variant {
    pub primary: usize,
    pub fallbacks: Vec<usize>,
}

pub struct CachedGlyph {
    pub width: usize,
    pub height: usize,
    pub xmin: i32,
    pub ymin: i32,
    pub advance: f32,
    pub coverage: Vec<u8>,
}

pub struct FontStore {
    fonts: Vec<fontdue::Font>,
    file_ids: HashMap<&'static str, usize>,
    /// the font table that actually loaded (Windows or Linux) — all
    /// family/variant lookups go through it
    table: &'static [(&'static str, bool, bool, &'static str)],
    /// runtime-registered web fonts (@font-face):
    /// (family, bold, italic) -> font slot; wins over the table
    dynamic: Vec<(String, bool, bool, usize)>,
    variants: Vec<Variant>,
    variant_ids: HashMap<(String, bool, bool), u32>,
    glyph_cache: HashMap<(usize, u32, char), CachedGlyph>,
    metrics_cache: HashMap<(u32, u32), (f32, f32, f32)>,
}

const FONT_DIR: &str = r"C:\Windows\Fonts";
const FONT_DIR_LINUX: &str = "/usr/share/fonts/truetype";

/// Linux fallback table (headless builds, CI): DejaVu stands in for
/// the Windows families under the same family keys, WenQuanYi covers
/// CJK. Missing files are skipped — file_font() falls back to slot 0.
const FONT_FILES_LINUX: &[(&str, bool, bool, &str)] = &[
    ("segoe ui", false, false, "dejavu/DejaVuSans.ttf"),
    ("segoe ui", true, false, "dejavu/DejaVuSans-Bold.ttf"),
    ("segoe ui", false, true, "dejavu/DejaVuSans-Oblique.ttf"),
    ("segoe ui", true, true, "dejavu/DejaVuSans-BoldOblique.ttf"),
    ("consolas", false, false, "dejavu/DejaVuSansMono.ttf"),
    ("consolas", true, false, "dejavu/DejaVuSansMono-Bold.ttf"),
    ("georgia", false, false, "dejavu/DejaVuSerif.ttf"),
    ("georgia", true, false, "dejavu/DejaVuSerif-Bold.ttf"),
    ("malgun gothic", false, false, "wqy/wqy-zenhei.ttc"),
    ("symbols", false, false, "dejavu/DejaVuSans.ttf"),
];

/// (family, bold, italic) -> file name
const FONT_FILES: &[(&str, bool, bool, &str)] = &[
    ("segoe ui", false, false, "segoeui.ttf"),
    ("segoe ui", true, false, "segoeuib.ttf"),
    ("segoe ui", false, true, "segoeuii.ttf"),
    ("segoe ui", true, true, "segoeuiz.ttf"),
    ("consolas", false, false, "consola.ttf"),
    ("consolas", true, false, "consolab.ttf"),
    ("consolas", false, true, "consolai.ttf"),
    ("consolas", true, true, "consolaz.ttf"),
    ("georgia", false, false, "georgia.ttf"),
    ("georgia", true, false, "georgiab.ttf"),
    ("georgia", false, true, "georgiai.ttf"),
    ("georgia", true, true, "georgiaz.ttf"),
    ("malgun gothic", false, false, "malgun.ttf"),
    ("malgun gothic", true, false, "malgunbd.ttf"),
    ("symbols", false, false, "seguisym.ttf"),
];

impl FontStore {
    pub fn new() -> Result<FontStore, String> {
        // Windows fonts first (the primary target), then the Linux
        // fallback table (headless/CI). Within a table, unreadable
        // files are skipped; a table counts only if its default
        // (first) font loaded.
        for (dir, table, sep) in [
            (FONT_DIR, FONT_FILES, '\\'),
            (FONT_DIR_LINUX, FONT_FILES_LINUX, '/'),
        ] {
            let mut fonts = Vec::new();
            let mut file_ids = HashMap::new();
            for &(_, _, _, file) in table {
                if file_ids.contains_key(file) {
                    continue;
                }
                let path = format!("{}{}{}", dir, sep, file);
                let Ok(data) = fs::read(&path) else { continue };
                let Ok(font) = fontdue::Font::from_bytes(
                    data,
                    fontdue::FontSettings::default(),
                ) else {
                    continue;
                };
                file_ids.insert(file, fonts.len());
                fonts.push(font);
            }
            if file_ids.contains_key(table[0].3) {
                return Ok(FontStore {
                    fonts,
                    file_ids,
                    table,
                    dynamic: Vec::new(),
                    variants: Vec::new(),
                    variant_ids: HashMap::new(),
                    glyph_cache: HashMap::new(),
                    metrics_cache: HashMap::new(),
                });
            }
        }
        Err("no usable font table (Windows or Linux)".to_string())
    }

    /// Register a web font (@font-face). Existing variant ids keep
    /// their old resolution; only the memo is cleared so future
    /// lookups (post-load relayout) see the new font.
    pub fn add_font(
        &mut self,
        family: &str,
        bold: bool,
        italic: bool,
        data: Vec<u8>,
    ) -> bool {
        let Ok(font) = fontdue::Font::from_bytes(
            data,
            fontdue::FontSettings::default(),
        ) else {
            return false;
        };
        let slot = self.fonts.len();
        self.fonts.push(font);
        self.dynamic.push((
            family.to_ascii_lowercase(),
            bold,
            italic,
            slot,
        ));
        self.variant_ids.clear();
        true
    }

    /// Is this family resolvable (table or web font)? The shell uses
    /// it to decide whether an author font-family passes through or
    /// collapses to the default family.
    pub fn has_family(&self, family: &str) -> bool {
        let f = family.to_ascii_lowercase();
        self.dynamic.iter().any(|(fam, ..)| fam == &f)
            || self.table.iter().any(|(fam, ..)| *fam == f)
    }

    fn file_font(&self, file: &str) -> usize {
        *self.file_ids.get(file).unwrap_or(&0)
    }

    fn lookup_variant_file(
        &self,
        family: &str,
        bold: bool,
        italic: bool,
    ) -> &'static str {
        // exact variant, then same family without italic/bold, then
        // the default family's closest variant
        for &(fam, b, i, file) in self.table {
            if fam == family && b == bold && i == italic {
                return file;
            }
        }
        for &(fam, b, _, file) in self.table {
            if fam == family && b == bold {
                return file;
            }
        }
        for &(fam, _, _, file) in self.table {
            if fam == family {
                return file;
            }
        }
        for &(fam, b, i, file) in self.table {
            if fam == "segoe ui" && b == bold && i == italic {
                return file;
            }
        }
        self.table[0].3
    }

    pub fn variant_id(&mut self, family: &str, bold: bool, italic: bool) -> u32 {
        let family = family.to_ascii_lowercase();
        let key = (family.clone(), bold, italic);
        if let Some(&id) = self.variant_ids.get(&key) {
            return id;
        }
        // web fonts win: exact variant, then same family
        let dyn_hit = self
            .dynamic
            .iter()
            .find(|(f, b, i, _)| f == &family && *b == bold && *i == italic)
            .or_else(|| {
                self.dynamic
                    .iter()
                    .find(|(f, b, _, _)| f == &family && *b == bold)
            })
            .or_else(|| {
                self.dynamic.iter().find(|(f, _, _, _)| f == &family)
            });
        let primary = match dyn_hit {
            Some(&(_, _, _, slot)) => slot,
            None => self
                .file_font(self.lookup_variant_file(&family, bold, italic)),
        };
        // Hangul/symbol fallbacks; prefer bold Malgun for bold variants
        let mut fallbacks = Vec::new();
        if bold {
            let f = self.lookup_variant_file("malgun gothic", true, false);
            fallbacks.push(self.file_font(f));
        }
        let malgun =
            self.lookup_variant_file("malgun gothic", false, false);
        fallbacks.push(self.file_font(malgun));
        let sym = self.lookup_variant_file("symbols", false, false);
        fallbacks.push(self.file_font(sym));
        fallbacks.retain(|&f| f != primary);

        let id = self.variants.len() as u32;
        self.variants.push(Variant { primary, fallbacks });
        self.variant_ids.insert(key, id);
        id
    }

    fn resolve(&self, variant: u32, c: char) -> Option<usize> {
        let v = self.variants.get(variant as usize)?;
        if self.fonts[v.primary].lookup_glyph_index(c) != 0 {
            return Some(v.primary);
        }
        for &f in &v.fallbacks {
            if self.fonts[f].lookup_glyph_index(c) != 0 {
                return Some(f);
            }
        }
        None
    }

    /// (ascent, descent, linespace) for a variant at a pixel size.
    pub fn metrics(&mut self, variant: u32, size: f32) -> (f32, f32, f32) {
        let key = (variant, (size * 4.0) as u32);
        if let Some(&m) = self.metrics_cache.get(&key) {
            return m;
        }
        let primary = self
            .variants
            .get(variant as usize)
            .map(|v| v.primary)
            .unwrap_or(0);
        let lm = self.fonts[primary]
            .horizontal_line_metrics(size)
            .unwrap_or(fontdue::LineMetrics {
                ascent: size * 0.8,
                descent: -size * 0.2,
                line_gap: 0.0,
                new_line_size: size,
            });
        let ascent = lm.ascent;
        let descent = -lm.descent;
        let linespace = ascent + descent + lm.line_gap;
        let m = (ascent, descent, linespace);
        self.metrics_cache.insert(key, m);
        m
    }

    pub fn measure(&mut self, variant: u32, size: f32, text: &str) -> f32 {
        let mut width = 0.0f32;
        let mut prev: Option<(usize, char)> = None;
        for mut c in text.chars() {
            if c == '\u{a0}' {
                c = ' ';
            }
            if c == '\t' {
                let sp = self.advance(variant, size, ' ');
                width += sp * 4.0;
                prev = None;
                continue;
            }
            match self.resolve(variant, c) {
                Some(f) => {
                    if let Some((pf, pc)) = prev {
                        if pf == f {
                            if let Some(k) =
                                self.fonts[f].horizontal_kern(pc, c, size)
                            {
                                width += k;
                            }
                        }
                    }
                    width += self.glyph(f, size, c).advance;
                    prev = Some((f, c));
                }
                None => {
                    width += size * 0.6; // missing glyph box
                    prev = None;
                }
            }
        }
        width
    }

    fn advance(&mut self, variant: u32, size: f32, c: char) -> f32 {
        match self.resolve(variant, c) {
            Some(f) => self.glyph(f, size, c).advance,
            None => size * 0.6,
        }
    }

    pub fn glyph(&mut self, font_idx: usize, size: f32, c: char) -> &CachedGlyph {
        let key = (font_idx, (size * 4.0) as u32, c);
        if !self.glyph_cache.contains_key(&key) {
            let (m, coverage) = self.fonts[font_idx].rasterize(c, size);
            self.glyph_cache.insert(
                key,
                CachedGlyph {
                    width: m.width,
                    height: m.height,
                    xmin: m.xmin,
                    ymin: m.ymin,
                    advance: m.advance_width,
                    coverage,
                },
            );
        }
        self.glyph_cache.get(&key).unwrap()
    }

    /// Per-char font resolution for the rasterizer.
    pub fn resolve_char(&self, variant: u32, c: char) -> Option<usize> {
        self.resolve(variant, c)
    }

    /// Kerning between two chars if both live in the same font.
    pub fn kern(&self, font_idx: usize, a: char, b: char, size: f32) -> f32 {
        self.fonts[font_idx]
            .horizontal_kern(a, b, size)
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_font_registration_dispatches() {
        let Ok(mut store) = FontStore::new() else {
            return; // no system fonts in this environment: skip
        };
        let serif = "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf";
        let Ok(data) = fs::read(serif) else {
            return; // Windows CI: table differs, skip
        };
        assert!(!store.has_family("myface"));
        assert!(store.add_font("MyFace", false, false, data));
        assert!(store.has_family("myface"));
        assert!(store.has_family("MyFace")); // case-insensitive
        // the web font actually renders: same text measures
        // differently than the default (sans) family
        let sans = store.variant_id("segoe ui", false, false);
        let web = store.variant_id("myface", false, false);
        let a = store.measure(sans, 16.0, "illustration");
        let b = store.measure(web, 16.0, "illustration");
        assert!((a - b).abs() > 0.5, "sans {a} vs web {b}");
        // unparsable data is rejected, store stays sane
        assert!(!store.add_font("bad", false, false, vec![1, 2, 3]));
        assert!(store.has_family("myface"));
    }
}
