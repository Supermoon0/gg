//! Minimal SVG path rasterizer: parses `d` attribute path data
//! (M/L/H/V/C/S/Q/T/A/Z, absolute and relative, implicit repeats),
//! flattens curves to polylines, and fills with the nonzero winding
//! rule at 4x4 supersampling. Enough for icon-style inline SVGs
//! (naver's search magnifier); no strokes, gradients, or clips.

const SS: usize = 4; // supersampling factor per axis
const CURVE_SEGS: usize = 20;

// ---------------------------------------------------------------- parse

struct PathScan<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> PathScan<'a> {
    fn new(d: &'a str) -> Self {
        PathScan { b: d.as_bytes(), i: 0 }
    }

    fn skip_sep(&mut self) {
        while self.i < self.b.len()
            && (self.b[self.i].is_ascii_whitespace() || self.b[self.i] == b',')
        {
            self.i += 1;
        }
    }

    fn peek_cmd(&mut self) -> Option<u8> {
        self.skip_sep();
        match self.b.get(self.i) {
            Some(&c) if c.is_ascii_alphabetic() => Some(c),
            _ => None,
        }
    }

    /// SVG allows "1.5.5" (= 1.5, .5) and "1-2" (= 1, -2): a number
    /// ends at a second '.' or an unexpected sign.
    fn number(&mut self) -> Option<f64> {
        self.skip_sep();
        let start = self.i;
        let n = self.b.len();
        if self.i < n && (self.b[self.i] == b'+' || self.b[self.i] == b'-') {
            self.i += 1;
        }
        let mut seen_dot = false;
        let mut seen_digit = false;
        while self.i < n {
            match self.b[self.i] {
                b'0'..=b'9' => {
                    seen_digit = true;
                    self.i += 1;
                }
                b'.' if !seen_dot => {
                    seen_dot = true;
                    self.i += 1;
                }
                b'e' | b'E' if seen_digit => {
                    let mut j = self.i + 1;
                    if j < n && (self.b[j] == b'+' || self.b[j] == b'-') {
                        j += 1;
                    }
                    if j < n && self.b[j].is_ascii_digit() {
                        self.i = j;
                        while self.i < n && self.b[self.i].is_ascii_digit() {
                            self.i += 1;
                        }
                    }
                    break;
                }
                _ => break,
            }
        }
        if !seen_digit {
            self.i = start;
            return None;
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()?
            .parse()
            .ok()
    }

    /// Arc flags are single characters and may be packed ("11" or
    /// "1 1" or even "1-2.5" where -2.5 is the next coordinate).
    fn flag(&mut self) -> Option<bool> {
        self.skip_sep();
        match self.b.get(self.i) {
            Some(b'0') => {
                self.i += 1;
                Some(false)
            }
            Some(b'1') => {
                self.i += 1;
                Some(true)
            }
            _ => None,
        }
    }
}

/// Flattened subpaths (closed polygons) in user/viewBox coordinates.
pub fn flatten_path(d: &str) -> Vec<Vec<(f64, f64)>> {
    let mut subpaths: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut poly: Vec<(f64, f64)> = Vec::new();
    let mut s = PathScan::new(d);
    let (mut cx, mut cy) = (0.0f64, 0.0f64);
    let (mut sx, mut sy) = (0.0f64, 0.0f64); // subpath start
    let mut last_cubic_ctrl: Option<(f64, f64)> = None;
    let mut last_quad_ctrl: Option<(f64, f64)> = None;
    let mut cmd = b' ';

    macro_rules! flush {
        () => {
            if poly.len() >= 3 {
                subpaths.push(std::mem::take(&mut poly));
            } else {
                poly.clear();
            }
        };
    }

    loop {
        if let Some(c) = s.peek_cmd() {
            s.i += 1;
            cmd = c;
        } else {
            // implicit repeat: probe for a number, then rewind
            s.skip_sep();
            let save = s.i;
            if s.number().is_none() {
                break; // end of data
            }
            s.i = save;
            // M/m repeats as L/l per spec
            cmd = match cmd {
                b'M' => b'L',
                b'm' => b'l',
                other => other,
            };
        }
        let rel = cmd.is_ascii_lowercase();
        match cmd.to_ascii_uppercase() {
            b'M' => {
                let (Some(x), Some(y)) = (s.number(), s.number()) else {
                    break;
                };
                flush!();
                cx = if rel { cx + x } else { x };
                cy = if rel { cy + y } else { y };
                sx = cx;
                sy = cy;
                poly.push((cx, cy));
                last_cubic_ctrl = None;
                last_quad_ctrl = None;
            }
            b'L' => {
                let (Some(x), Some(y)) = (s.number(), s.number()) else {
                    break;
                };
                cx = if rel { cx + x } else { x };
                cy = if rel { cy + y } else { y };
                poly.push((cx, cy));
                last_cubic_ctrl = None;
                last_quad_ctrl = None;
            }
            b'H' => {
                let Some(x) = s.number() else { break };
                cx = if rel { cx + x } else { x };
                poly.push((cx, cy));
                last_cubic_ctrl = None;
                last_quad_ctrl = None;
            }
            b'V' => {
                let Some(y) = s.number() else { break };
                cy = if rel { cy + y } else { y };
                poly.push((cx, cy));
                last_cubic_ctrl = None;
                last_quad_ctrl = None;
            }
            b'C' | b'S' => {
                let (x1, y1) = if cmd.to_ascii_uppercase() == b'C' {
                    let (Some(a), Some(b)) = (s.number(), s.number())
                    else {
                        break;
                    };
                    (if rel { cx + a } else { a }, if rel { cy + b } else { b })
                } else {
                    // S: first control point mirrors the previous one
                    match last_cubic_ctrl {
                        Some((px, py)) => (2.0 * cx - px, 2.0 * cy - py),
                        None => (cx, cy),
                    }
                };
                let (Some(a2), Some(b2), Some(a3), Some(b3)) =
                    (s.number(), s.number(), s.number(), s.number())
                else {
                    break;
                };
                let (x2, y2) = (
                    if rel { cx + a2 } else { a2 },
                    if rel { cy + b2 } else { b2 },
                );
                let (x3, y3) = (
                    if rel { cx + a3 } else { a3 },
                    if rel { cy + b3 } else { b3 },
                );
                for k in 1..=CURVE_SEGS {
                    let t = k as f64 / CURVE_SEGS as f64;
                    let mt = 1.0 - t;
                    let x = mt * mt * mt * cx
                        + 3.0 * mt * mt * t * x1
                        + 3.0 * mt * t * t * x2
                        + t * t * t * x3;
                    let y = mt * mt * mt * cy
                        + 3.0 * mt * mt * t * y1
                        + 3.0 * mt * t * t * y2
                        + t * t * t * y3;
                    poly.push((x, y));
                }
                last_cubic_ctrl = Some((x2, y2));
                last_quad_ctrl = None;
                cx = x3;
                cy = y3;
            }
            b'Q' | b'T' => {
                let (x1, y1) = if cmd.to_ascii_uppercase() == b'Q' {
                    let (Some(a), Some(b)) = (s.number(), s.number())
                    else {
                        break;
                    };
                    (if rel { cx + a } else { a }, if rel { cy + b } else { b })
                } else {
                    match last_quad_ctrl {
                        Some((px, py)) => (2.0 * cx - px, 2.0 * cy - py),
                        None => (cx, cy),
                    }
                };
                let (Some(a2), Some(b2)) = (s.number(), s.number()) else {
                    break;
                };
                let (x2, y2) = (
                    if rel { cx + a2 } else { a2 },
                    if rel { cy + b2 } else { b2 },
                );
                for k in 1..=CURVE_SEGS {
                    let t = k as f64 / CURVE_SEGS as f64;
                    let mt = 1.0 - t;
                    let x = mt * mt * cx + 2.0 * mt * t * x1 + t * t * x2;
                    let y = mt * mt * cy + 2.0 * mt * t * y1 + t * t * y2;
                    poly.push((x, y));
                }
                last_quad_ctrl = Some((x1, y1));
                last_cubic_ctrl = None;
                cx = x2;
                cy = y2;
            }
            b'A' => {
                let (Some(rx), Some(ry), Some(rot)) =
                    (s.number(), s.number(), s.number())
                else {
                    break;
                };
                let (Some(large), Some(sweep)) = (s.flag(), s.flag())
                else {
                    break;
                };
                let (Some(a), Some(b)) = (s.number(), s.number()) else {
                    break;
                };
                let (ex, ey) = (
                    if rel { cx + a } else { a },
                    if rel { cy + b } else { b },
                );
                arc_to_polyline(
                    &mut poly, cx, cy, rx, ry, rot, large, sweep, ex, ey,
                );
                last_cubic_ctrl = None;
                last_quad_ctrl = None;
                cx = ex;
                cy = ey;
            }
            b'Z' => {
                if !poly.is_empty() {
                    cx = sx;
                    cy = sy;
                }
                flush!();
                last_cubic_ctrl = None;
                last_quad_ctrl = None;
            }
            _ => break, // unknown command: stop parsing gracefully
        }
    }
    flush!();
    subpaths
}

/// SVG elliptical arc -> polyline (endpoint to center parameterization,
/// spec appendix F.6).
#[allow(clippy::too_many_arguments)]
fn arc_to_polyline(
    poly: &mut Vec<(f64, f64)>,
    x1: f64,
    y1: f64,
    rx: f64,
    ry: f64,
    rot_deg: f64,
    large: bool,
    sweep: bool,
    x2: f64,
    y2: f64,
) {
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx < 1e-9 || ry < 1e-9 || (x1 == x2 && y1 == y2) {
        poly.push((x2, y2));
        return;
    }
    let phi = rot_deg.to_radians();
    let (cos_p, sin_p) = (phi.cos(), phi.sin());
    let dx2 = (x1 - x2) / 2.0;
    let dy2 = (y1 - y2) / 2.0;
    let x1p = cos_p * dx2 + sin_p * dy2;
    let y1p = -sin_p * dx2 + cos_p * dy2;
    // scale radii up if the endpoints cannot be reached
    let lam = x1p * x1p / (rx * rx) + y1p * y1p / (ry * ry);
    if lam > 1.0 {
        let s = lam.sqrt();
        rx *= s;
        ry *= s;
    }
    let num = (rx * rx * ry * ry - rx * rx * y1p * y1p
        - ry * ry * x1p * x1p)
        .max(0.0);
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let mut coef = if den > 0.0 { (num / den).sqrt() } else { 0.0 };
    if large == sweep {
        coef = -coef;
    }
    let cxp = coef * rx * y1p / ry;
    let cyp = -coef * ry * x1p / rx;
    let cx = cos_p * cxp - sin_p * cyp + (x1 + x2) / 2.0;
    let cy = sin_p * cxp + cos_p * cyp + (y1 + y2) / 2.0;

    let angle = |ux: f64, uy: f64, vx: f64, vy: f64| -> f64 {
        let dot = ux * vx + uy * vy;
        let len = (ux * ux + uy * uy).sqrt() * (vx * vx + vy * vy).sqrt();
        let mut a = (dot / len).clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 {
            a = -a;
        }
        a
    };
    let theta1 = angle(1.0, 0.0, (x1p - cxp) / rx, (y1p - cyp) / ry);
    let mut dtheta = angle(
        (x1p - cxp) / rx,
        (y1p - cyp) / ry,
        (-x1p - cxp) / rx,
        (-y1p - cyp) / ry,
    );
    if !sweep && dtheta > 0.0 {
        dtheta -= 2.0 * std::f64::consts::PI;
    } else if sweep && dtheta < 0.0 {
        dtheta += 2.0 * std::f64::consts::PI;
    }
    let segs = ((dtheta.abs() / (std::f64::consts::PI / 16.0)).ceil()
        as usize)
        .max(2);
    for k in 1..=segs {
        let t = theta1 + dtheta * (k as f64 / segs as f64);
        let (ct, st) = (t.cos(), t.sin());
        let x = cos_p * rx * ct - sin_p * ry * st + cx;
        let y = sin_p * rx * ct + cos_p * ry * st + cy;
        poly.push((x, y));
    }
}

// ----------------------------------------------------------------- fill

/// Rasterize filled paths into an RGBA buffer of out_w x out_h.
/// view_box maps user coordinates onto the output rectangle.
pub fn rasterize(
    view_box: (f64, f64, f64, f64),
    out_w: usize,
    out_h: usize,
    paths: &[(String, (u8, u8, u8))],
) -> Vec<u8> {
    let (vx, vy, vw, vh) = view_box;
    let mut rgba = vec![0u8; out_w * out_h * 4];
    if vw <= 0.0 || vh <= 0.0 || out_w == 0 || out_h == 0 {
        return rgba;
    }
    let sx = (out_w * SS) as f64 / vw;
    let sy = (out_h * SS) as f64 / vh;
    let mut cov = vec![0u16; out_w * out_h];

    for (d, (r, g, b)) in paths {
        let subpaths = flatten_path(d);
        if subpaths.is_empty() {
            continue;
        }
        // edges in supersampled device space
        let mut edges: Vec<(f64, f64, f64, f64)> = Vec::new();
        for sp in &subpaths {
            for i in 0..sp.len() {
                let (ax, ay) = sp[i];
                let (bx, by) = sp[(i + 1) % sp.len()];
                let e = (
                    (ax - vx) * sx,
                    (ay - vy) * sy,
                    (bx - vx) * sx,
                    (by - vy) * sy,
                );
                if e.1 != e.3 {
                    edges.push(e);
                }
            }
        }
        if edges.is_empty() {
            continue;
        }
        cov.iter_mut().for_each(|c| *c = 0);
        let mut xs: Vec<(f64, i32)> = Vec::new();
        for sub_y in 0..out_h * SS {
            let yc = sub_y as f64 + 0.5;
            xs.clear();
            for &(ax, ay, bx, by) in &edges {
                let (top, bot) = if ay < by { (ay, by) } else { (by, ay) };
                if yc < top || yc >= bot {
                    continue;
                }
                let x = ax + (yc - ay) * (bx - ax) / (by - ay);
                xs.push((x, if by > ay { 1 } else { -1 }));
            }
            if xs.is_empty() {
                continue;
            }
            xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            let row = (sub_y / SS) * out_w;
            let mut wind = 0;
            let mut span_start = 0.0f64;
            for &(x, dir) in xs.iter() {
                if wind != 0 {
                    // nonzero span [span_start, x) in subpixel coords
                    let a = span_start.max(0.0);
                    let b = x.min((out_w * SS) as f64);
                    let mut sub = a;
                    while sub < b {
                        let px = (sub as usize) / SS;
                        let cell_end =
                            (((sub as usize) / SS + 1) * SS) as f64;
                        let step = cell_end.min(b) - sub;
                        cov[row + px] += step.max(0.0) as u16;
                        sub = cell_end;
                    }
                }
                wind += dir;
                span_start = x;
            }
        }
        // composite this path's coverage as src-over
        let full = (SS * SS) as u32;
        for (i, &c) in cov.iter().enumerate() {
            if c == 0 {
                continue;
            }
            let alpha = ((c as u32).min(full) * 255 / full) as u8;
            let p = i * 4;
            let (dr, dg, db, da) =
                (rgba[p], rgba[p + 1], rgba[p + 2], rgba[p + 3]);
            let sa = alpha as u32;
            let inv = 255 - sa;
            rgba[p] = ((*r as u32 * sa + dr as u32 * inv) / 255) as u8;
            rgba[p + 1] = ((*g as u32 * sa + dg as u32 * inv) / 255) as u8;
            rgba[p + 2] = ((*b as u32 * sa + db as u32 * inv) / 255) as u8;
            rgba[p + 3] = (sa + da as u32 * inv / 255).min(255) as u8;
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numbers_and_implicit_commands() {
        // "1.5.5" is two numbers; "M 0 0 10 0 10 10" repeats as L
        let sp = flatten_path("M0 0 10 0 10 10Z");
        assert_eq!(sp.len(), 1);
        assert_eq!(sp[0].len(), 3);
        // compressed numbers: "1.5.5" = 1.5, .5 and "2-1" = 2, -1
        let sp = flatten_path("M1.5.5L2-1 3 4z");
        assert_eq!(sp[0][0], (1.5, 0.5));
        assert_eq!(sp[0][1], (2.0, -1.0));
        assert_eq!(sp[0][2], (3.0, 4.0));
    }

    #[test]
    fn fills_a_square_with_coverage() {
        // unit square over a 10x10 viewBox fills 1/4 of a 2x2 output
        let rgba = rasterize(
            (0.0, 0.0, 10.0, 10.0),
            2,
            2,
            &[("M0 0H5V5H0Z".to_string(), (255, 0, 0))],
        );
        assert_eq!(rgba[3], 255, "top-left pixel fully covered");
        assert_eq!(rgba[0], 255, "red channel");
        assert_eq!(rgba[4 + 3], 0, "top-right pixel empty");
    }

    #[test]
    fn curves_and_arcs_flatten() {
        let sp = flatten_path("M0 0C0 10 10 10 10 0Z");
        assert!(sp[0].len() > 10);
        // naver-style compressed arc args: "a2.41 2.41 0 0 1-1.7 4.1"
        let sp = flatten_path("M10 10a2.41 2.41 0 0 1-1.7 4.1Z");
        assert!(sp[0].len() > 3);
        // a circle via two arcs covers most of its bounding box row
        let rgba = rasterize(
            (0.0, 0.0, 10.0, 10.0),
            10,
            10,
            &[(
                "M5 1A4 4 0 1 1 4.99 1Z".to_string(),
                (0, 255, 0),
            )],
        );
        let mid = (5 * 10 + 5) * 4;
        assert!(rgba[mid + 3] > 200, "circle center covered");
    }
}
