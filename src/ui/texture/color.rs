//! colorsys-compatible HLS <-> RGB and HSV helpers.

#[inline]
pub fn rgb_to_hls(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let maxc = r.max(g).max(b);
    let minc = r.min(g).min(b);
    let sumc = maxc + minc;
    let rangec = maxc - minc;
    let l = sumc / 2.0;
    if minc == maxc {
        return (0.0, l, 0.0);
    }
    let s = if l <= 0.5 {
        rangec / sumc
    } else {
        rangec / (2.0 - maxc - minc)
    };
    let rc = (maxc - r) / rangec;
    let gc = (maxc - g) / rangec;
    let bc = (maxc - b) / rangec;
    let h = if r == maxc {
        bc - gc
    } else if g == maxc {
        2.0 + rc - bc
    } else {
        4.0 + gc - rc
    };
    ((h / 6.0).rem_euclid(1.0), l, s)
}

#[inline]
fn hue_channel(h: f64, m1: f64, m2: f64) -> f64 {
    let h = h.rem_euclid(1.0);
    if h < 1.0 / 6.0 {
        m1 + (m2 - m1) * h * 6.0
    } else if h < 0.5 {
        m2
    } else if h < 2.0 / 3.0 {
        m1 + (m2 - m1) * (2.0 / 3.0 - h) * 6.0
    } else {
        m1
    }
}

pub fn hls_to_rgb(h: f64, l: f64, s: f64) -> (f64, f64, f64) {
    if s == 0.0 {
        return (l, l, l);
    }
    let m2 = if l <= 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let m1 = 2.0 * l - m2;
    (
        hue_channel(h + 1.0 / 3.0, m1, m2),
        hue_channel(h, m1, m2),
        hue_channel(h - 1.0 / 3.0, m1, m2),
    )
}

#[inline]
pub fn hls_rgb255(h: f64, l: f64, s: f64) -> [f32; 3] {
    let (r, g, b) = hls_to_rgb(h, l, s);
    [(r * 255.0) as f32, (g * 255.0) as f32, (b * 255.0) as f32]
}

/// Per-pixel HSV (h, s, v), each 0..1.
#[inline]
pub fn hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let (r, g, b) = (r / 255.0, g / 255.0, b / 255.0);
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let delta = mx - mn;
    let safe = delta.max(1e-6);
    let sat = if mx > 1e-6 { delta / mx.max(1e-6) } else { 0.0 };
    let mut h = 0.0;
    if mx == r {
        h = ((g - b) / safe).rem_euclid(6.0);
    }
    if mx == g && mx != r {
        h = (b - r) / safe + 2.0;
    }
    if mx == b && mx != r && mx != g {
        h = (r - g) / safe + 4.0;
    }
    ((h / 6.0).rem_euclid(1.0), sat, mx)
}
