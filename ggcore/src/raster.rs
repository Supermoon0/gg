//! Software rasterizer: draws the display list into an RGB buffer.

use crate::fonts::FontStore;

/// Clamped integer pixel range [a, b) within a canvas dimension.
fn px_range(a: f64, b: f64, limit: usize) -> std::ops::Range<i32> {
    let lo = (a.floor() as i32).max(0);
    let hi = (b.ceil() as i32).min(limit as i32);
    lo..hi.max(lo)
}

/// Colour at position `t` along a sorted stop list, clamped at the ends.
fn sample_stops(stops: &[(f64, (u8, u8, u8))], t: f64) -> (u8, u8, u8) {
    if t <= stops[0].0 {
        return stops[0].1;
    }
    let last = stops[stops.len() - 1];
    if t >= last.0 {
        return last.1;
    }
    for pair in stops.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if t <= b.0 {
            let span = b.0 - a.0;
            // a hard stop (two stops at the same offset) is the sharp
            // colour boundary that fakes stripes and checkerboards
            let f = if span <= 0.0 { 1.0 } else { (t - a.0) / span };
            return (
                lerp(a.1.0, b.1.0, f),
                lerp(a.1.1, b.1.1, f),
                lerp(a.1.2, b.1.2, f),
            );
        }
    }
    last.1
}

fn lerp(a: u8, b: u8, f: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * f).round().clamp(0.0, 255.0) as u8
}

/// Is (px, py) inside a rounded rect? Corners only -- the straight
/// edges are a plain bounds test.
fn inside_round_rect(
    px: f64,
    py: f64,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    r: f64,
) -> bool {
    if px < x1 || py < y1 || px > x2 || py > y2 {
        return false;
    }
    let cx = if px < x1 + r {
        x1 + r
    } else if px > x2 - r {
        x2 - r
    } else {
        return true;
    };
    let cy = if py < y1 + r {
        y1 + r
    } else if py > y2 - r {
        y2 - r
    } else {
        return true;
    };
    let (dx, dy) = (px - cx, py - cy);
    dx * dx + dy * dy <= r * r
}

pub struct Raster {
    pub width: usize,
    pub height: usize,
    pub buf: Vec<u8>, // RGB
    /// current clip rect (x1, y1, x2, y2); pixels outside are skipped
    clip: (i32, i32, i32, i32),
}

impl Raster {
    pub fn new(width: usize, height: usize, bg: (u8, u8, u8)) -> Raster {
        let mut buf = Vec::with_capacity(width * height * 3);
        for _ in 0..width * height {
            buf.push(bg.0);
            buf.push(bg.1);
            buf.push(bg.2);
        }
        Raster {
            width,
            height,
            buf,
            clip: (0, 0, width as i32, height as i32),
        }
    }

    pub fn clip(&self) -> (i32, i32, i32, i32) {
        self.clip
    }

    /// Intersect the clip with a new rect (rounded), returning the
    /// previous clip so the caller can restore it.
    pub fn push_clip(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
    ) -> (i32, i32, i32, i32) {
        let prev = self.clip;
        self.clip = (
            (x1.round() as i32).max(prev.0),
            (y1.round() as i32).max(prev.1),
            (x2.round() as i32).min(prev.2),
            (y2.round() as i32).min(prev.3),
        );
        prev
    }

    pub fn set_clip(&mut self, c: (i32, i32, i32, i32)) {
        self.clip = c;
    }

    #[inline]
    fn blend(&mut self, x: i32, y: i32, color: (u8, u8, u8), alpha: u8) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32
        {
            return;
        }
        if x < self.clip.0 || y < self.clip.1
            || x >= self.clip.2 || y >= self.clip.3
        {
            return;
        }
        if alpha == 0 {
            return;
        }
        let i = (y as usize * self.width + x as usize) * 3;
        if alpha == 255 {
            self.buf[i] = color.0;
            self.buf[i + 1] = color.1;
            self.buf[i + 2] = color.2;
            return;
        }
        let a = alpha as u32;
        let na = 255 - a;
        self.buf[i] = ((self.buf[i] as u32 * na + color.0 as u32 * a) / 255) as u8;
        self.buf[i + 1] =
            ((self.buf[i + 1] as u32 * na + color.1 as u32 * a) / 255) as u8;
        self.buf[i + 2] =
            ((self.buf[i + 2] as u32 * na + color.2 as u32 * a) / 255) as u8;
    }

    pub fn fill_rect(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, color: (u8, u8, u8)) {
        let x1 = (x1.round() as i32).max(0).max(self.clip.0);
        let y1 = (y1.round() as i32).max(0).max(self.clip.1);
        let x2 = (x2.round() as i32).min(self.width as i32).min(self.clip.2);
        let y2 = (y2.round() as i32).min(self.height as i32).min(self.clip.3);
        for y in y1..y2 {
            let row = y as usize * self.width;
            for x in x1..x2 {
                let i = (row + x as usize) * 3;
                self.buf[i] = color.0;
                self.buf[i + 1] = color.1;
                self.buf[i + 2] = color.2;
            }
        }
    }

    /// Rounded-corner rect (border-radius). Corner coverage is
    /// antialiased with a signed-distance test per pixel.
    pub fn fill_round_rect(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        color: (u8, u8, u8),
        radius: f64,
    ) {
        let r = radius.min((x2 - x1) / 2.0).min((y2 - y1) / 2.0);
        if r < 0.5 {
            self.fill_rect(x1, y1, x2, y2, color);
            return;
        }
        // three plain rects cover everything except the four corners
        self.fill_rect(x1 + r, y1, x2 - r, y2, color);
        self.fill_rect(x1, y1 + r, x1 + r, y2 - r, color);
        self.fill_rect(x2 - r, y1 + r, x2, y2 - r, color);
        for (cx, cy, sx, sy) in [
            (x1 + r, y1 + r, -1.0, -1.0),
            (x2 - r, y1 + r, 1.0, -1.0),
            (x1 + r, y2 - r, -1.0, 1.0),
            (x2 - r, y2 - r, 1.0, 1.0),
        ] {
            let px0 = (cx + sx * r).min(cx);
            let px1 = (cx + sx * r).max(cx);
            let py0 = (cy + sy * r).min(cy);
            let py1 = (cy + sy * r).max(cy);
            for y in px_range(py0, py1, self.height) {
                for x in px_range(px0, px1, self.width) {
                    let dx = x as f64 + 0.5 - cx;
                    let dy = y as f64 + 0.5 - cy;
                    let d = (dx * dx + dy * dy).sqrt();
                    // 1px feather at the arc edge
                    let a = (r - d + 0.5).clamp(0.0, 1.0);
                    if a > 0.0 {
                        self.blend(x, y, color, (a * 255.0) as u8);
                    }
                }
            }
        }
    }

    /// One CSS background layer: the image scaled to (tile_w, tile_h),
    /// placed at (off_x, off_y) inside the box, tiled on the repeat
    /// axes, everything clipped to the box.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_image_tiled(
        &mut self,
        src_w: u32,
        src_h: u32,
        rgba: &[u8],
        bx: f64,
        by: f64,
        bw: f64,
        bh: f64,
        off_x: f64,
        off_y: f64,
        tile_w: f64,
        tile_h: f64,
        rep_x: bool,
        rep_y: bool,
    ) {
        if src_w == 0 || src_h == 0 || tile_w < 1.0 || tile_h < 1.0
            || bw < 1.0 || bh < 1.0
        {
            return;
        }
        let clip = (
            (bx.round() as i32).max(0),
            (by.round() as i32).max(0),
            ((bx + bw).round() as i32).min(self.width as i32),
            ((by + bh).round() as i32).min(self.height as i32),
        );
        let mut ty = by + off_y;
        if rep_y {
            while ty > by {
                ty -= tile_h;
            }
        }
        let y_end = if rep_y { by + bh } else { ty + 1.0 };
        while ty < y_end {
            let mut tx = bx + off_x;
            if rep_x {
                while tx > bx {
                    tx -= tile_w;
                }
            }
            let x_end = if rep_x { bx + bw } else { tx + 1.0 };
            while tx < x_end {
                self.draw_image_clipped(
                    src_w, src_h, rgba, tx, ty, tile_w, tile_h, clip,
                );
                if !rep_x {
                    break;
                }
                tx += tile_w;
            }
            if !rep_y {
                break;
            }
            ty += tile_h;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_image_clipped(
        &mut self,
        src_w: u32,
        src_h: u32,
        rgba: &[u8],
        x: f64,
        y: f64,
        dst_w: f64,
        dst_h: f64,
        clip: (i32, i32, i32, i32),
    ) {
        if dst_w < 1.0 || dst_h < 1.0 {
            return;
        }
        let x0 = x.round() as i32;
        let y0 = y.round() as i32;
        let dw = dst_w.round() as i32;
        let dh = dst_h.round() as i32;
        let sx_step = src_w as f64 / dw as f64;
        let sy_step = src_h as f64 / dh as f64;
        let dy_lo = (clip.1 - y0).max(0);
        let dy_hi = (clip.3 - y0).min(dh);
        let dx_lo = (clip.0 - x0).max(0);
        let dx_hi = (clip.2 - x0).min(dw);
        for dy in dy_lo..dy_hi {
            let py = y0 + dy;
            let sy = (dy as f64 + 0.5) * sy_step - 0.5;
            let sy0 = sy.floor().max(0.0) as u32;
            let sy1 = (sy0 + 1).min(src_h - 1);
            let fy = (sy - sy0 as f64).clamp(0.0, 1.0);
            for dx in dx_lo..dx_hi {
                let px = x0 + dx;
                let sx = (dx as f64 + 0.5) * sx_step - 0.5;
                let sx0 = sx.floor().max(0.0) as u32;
                let sx1 = (sx0 + 1).min(src_w - 1);
                let fx = (sx - sx0 as f64).clamp(0.0, 1.0);
                let sample = |xx: u32, yy: u32| -> [f64; 4] {
                    let i = ((yy * src_w + xx) * 4) as usize;
                    [
                        rgba[i] as f64,
                        rgba[i + 1] as f64,
                        rgba[i + 2] as f64,
                        rgba[i + 3] as f64,
                    ]
                };
                let p00 = sample(sx0, sy0);
                let p10 = sample(sx1, sy0);
                let p01 = sample(sx0, sy1);
                let p11 = sample(sx1, sy1);
                let mut out = [0f64; 4];
                for c in 0..4 {
                    let top = p00[c] * (1.0 - fx) + p10[c] * fx;
                    let bot = p01[c] * (1.0 - fx) + p11[c] * fx;
                    out[c] = top * (1.0 - fy) + bot * fy;
                }
                self.blend(
                    px,
                    py,
                    (out[0] as u8, out[1] as u8, out[2] as u8),
                    out[3] as u8,
                );
            }
        }
    }

    pub fn draw_line(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        color: (u8, u8, u8),
        thickness: f64,
    ) {
        let t = thickness.max(1.0);
        if (y1 - y2).abs() < 0.5 {
            // horizontal
            let (a, b) = if x1 <= x2 { (x1, x2) } else { (x2, x1) };
            self.fill_rect(a, y1 - t / 2.0, b, y1 + t / 2.0, color);
        } else if (x1 - x2).abs() < 0.5 {
            let (a, b) = if y1 <= y2 { (y1, y2) } else { (y2, y1) };
            self.fill_rect(x1 - t / 2.0, a, x1 + t / 2.0, b, color);
        } else {
            // DDA for the rare diagonal
            let steps = (x2 - x1).abs().max((y2 - y1).abs()).ceil() as i32;
            for s in 0..=steps {
                let f = s as f64 / steps as f64;
                let x = x1 + (x2 - x1) * f;
                let y = y1 + (y2 - y1) * f;
                self.blend(x.round() as i32, y.round() as i32, color, 255);
            }
        }
    }

    pub fn fill_oval(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, color: (u8, u8, u8)) {
        let cx = (x1 + x2) / 2.0;
        let cy = (y1 + y2) / 2.0;
        let rx = ((x2 - x1) / 2.0).max(0.5);
        let ry = ((y2 - y1) / 2.0).max(0.5);
        let ix1 = x1.floor() as i32;
        let iy1 = y1.floor() as i32;
        let ix2 = x2.ceil() as i32;
        let iy2 = y2.ceil() as i32;
        for y in iy1..=iy2 {
            for x in ix1..=ix2 {
                // 2x2 supersample for soft edges
                let mut hits = 0;
                for (ox, oy) in
                    [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)]
                {
                    let dx = (x as f64 + ox - cx) / rx;
                    let dy = (y as f64 + oy - cy) / ry;
                    if dx * dx + dy * dy <= 1.0 {
                        hits += 1;
                    }
                }
                if hits > 0 {
                    self.blend(x, y, color, (hits * 255 / 4) as u8);
                }
            }
        }
    }

    /// Fill a box with a linear gradient.
    ///
    /// `angle` follows the CSS convention, not the mathematical one: 0
    /// points up and it turns clockwise, so 180deg runs top-to-bottom,
    /// which is what a bare `linear-gradient(a, b)` means. Stops are
    /// (offset in 0..=1, colour) and must already be sorted and
    /// resolved -- the parser owns the defaulting rules, this owns the
    /// pixels.
    ///
    /// The gradient line is sized so the corners of the box land on
    /// its ends (CSS Images 3 s3.4.1): the projection of the box's
    /// half-diagonal onto the gradient direction.
    pub fn fill_linear_gradient(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        angle_deg: f64,
        stops: &[(f64, (u8, u8, u8))],
        radius: f64,
    ) {
        if stops.is_empty() {
            return;
        }
        if stops.len() == 1 {
            if radius > 0.5 {
                self.fill_round_rect(x1, y1, x2, y2, stops[0].1, radius);
            } else {
                self.fill_rect(x1, y1, x2, y2, stops[0].1);
            }
            return;
        }
        let (w, h) = (x2 - x1, y2 - y1);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let rad = angle_deg.to_radians();
        // CSS angle -> unit vector, y growing downward
        let (dx, dy) = (rad.sin(), -rad.cos());
        let len = (w * dx).abs() + (h * dy).abs();
        if len <= 0.0 {
            return;
        }
        let (cx, cy) = ((x1 + x2) / 2.0, (y1 + y2) / 2.0);
        // distance along the gradient line of the starting corner
        let start = -len / 2.0;
        let xs = px_range(x1, x2, self.width);
        let ys = px_range(y1, y2, self.height);
        let rr = radius.min(w / 2.0).min(h / 2.0);
        for y in ys {
            for x in xs.clone() {
                let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
                if rr > 0.5 && !inside_round_rect(px, py, x1, y1, x2, y2, rr)
                {
                    continue;
                }
                let t = (((px - cx) * dx + (py - cy) * dy) - start) / len;
                self.blend(x, y, sample_stops(stops, t), 255);
            }
        }
    }

    pub fn draw_text(
        &mut self,
        store: &mut FontStore,
        x: f64,
        y_top: f64,
        variant: u32,
        size: f32,
        color: (u8, u8, u8),
        text: &str,
    ) {
        let (ascent, _d, _ls) = store.metrics(variant, size);
        let baseline = y_top as f32 + ascent;
        let mut pen = x as f32;
        let mut prev: Option<(usize, char)> = None;

        for mut c in text.chars() {
            if c == '\u{a0}' {
                c = ' ';
            }
            if c == '\t' {
                if let Some(f) = store.resolve_char(variant, ' ') {
                    pen += store.glyph(f, size, ' ').advance * 4.0;
                }
                prev = None;
                continue;
            }
            let font_idx = match store.resolve_char(variant, c) {
                Some(f) => f,
                None => {
                    // missing glyph: hollow box
                    let w = size as f64 * 0.5;
                    let bx = pen as f64 + 1.0;
                    let by = baseline as f64 - size as f64 * 0.7;
                    self.fill_rect(bx, by, bx + w, by + 1.0, color);
                    self.fill_rect(bx, by + size as f64 * 0.6, bx + w,
                                   by + size as f64 * 0.6 + 1.0, color);
                    self.fill_rect(bx, by, bx + 1.0,
                                   by + size as f64 * 0.6 + 1.0, color);
                    self.fill_rect(bx + w - 1.0, by, bx + w,
                                   by + size as f64 * 0.6 + 1.0, color);
                    pen += size * 0.6;
                    prev = None;
                    continue;
                }
            };
            if let Some((pf, pc)) = prev {
                if pf == font_idx {
                    pen += store.kern(font_idx, pc, c, size);
                }
            }
            let g = store.glyph(font_idx, size, c);
            let gx = pen.round() as i32 + g.xmin;
            let gy = baseline.round() as i32 - g.height as i32 - g.ymin;
            for row in 0..g.height {
                for col in 0..g.width {
                    let a = g.coverage[row * g.width + col];
                    if a > 0 {
                        self.blend(gx + col as i32, gy + row as i32, color, a);
                    }
                }
            }
            pen += g.advance;
            prev = Some((font_idx, c));
        }
    }

    /// Blit an RGBA image, bilinear-scaled to (w, h), alpha-blended.
    pub fn draw_image(
        &mut self,
        src_w: u32,
        src_h: u32,
        rgba: &[u8],
        x: f64,
        y: f64,
        dst_w: f64,
        dst_h: f64,
    ) {
        if src_w == 0 || src_h == 0 || dst_w < 1.0 || dst_h < 1.0 {
            return;
        }
        let x0 = x.round() as i32;
        let y0 = y.round() as i32;
        let dw = dst_w.round() as i32;
        let dh = dst_h.round() as i32;
        let sx_step = src_w as f64 / dw as f64;
        let sy_step = src_h as f64 / dh as f64;
        for dy in 0..dh {
            let py = y0 + dy;
            if py < 0 || py >= self.height as i32 {
                continue;
            }
            let sy = (dy as f64 + 0.5) * sy_step - 0.5;
            let sy0 = sy.floor().max(0.0) as u32;
            let sy1 = (sy0 + 1).min(src_h - 1);
            let fy = (sy - sy0 as f64).clamp(0.0, 1.0);
            for dx in 0..dw {
                let px = x0 + dx;
                if px < 0 || px >= self.width as i32 {
                    continue;
                }
                let sx = (dx as f64 + 0.5) * sx_step - 0.5;
                let sx0 = sx.floor().max(0.0) as u32;
                let sx1 = (sx0 + 1).min(src_w - 1);
                let fx = (sx - sx0 as f64).clamp(0.0, 1.0);

                let sample = |xx: u32, yy: u32| -> [f64; 4] {
                    let i = ((yy * src_w + xx) * 4) as usize;
                    [
                        rgba[i] as f64,
                        rgba[i + 1] as f64,
                        rgba[i + 2] as f64,
                        rgba[i + 3] as f64,
                    ]
                };
                let p00 = sample(sx0, sy0);
                let p10 = sample(sx1, sy0);
                let p01 = sample(sx0, sy1);
                let p11 = sample(sx1, sy1);
                let mut out = [0f64; 4];
                for c in 0..4 {
                    let top = p00[c] * (1.0 - fx) + p10[c] * fx;
                    let bot = p01[c] * (1.0 - fx) + p11[c] * fx;
                    out[c] = top * (1.0 - fy) + bot * fy;
                }
                self.blend(
                    px,
                    py,
                    (out[0] as u8, out[1] as u8, out[2] as u8),
                    out[3] as u8,
                );
            }
        }
    }

    /// Encode as binary PPM (P6) for tkinter PhotoImage.
    pub fn to_ppm(&self) -> Vec<u8> {
        let header = format!("P6\n{} {}\n255\n", self.width, self.height);
        let mut out = Vec::with_capacity(header.len() + self.buf.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&self.buf);
        out
    }
}
