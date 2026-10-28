//! Per-pixel and per-dab kernels for the toning and focus retouching tools: Dodge, Burn, Sponge,
//! Blur, Sharpen. The brush engine (`niveta-paint`) decides *where* and *how strongly* each dab
//! applies; these functions decide *what* happens to the pixels.
//!
//! Tone curves are our own design, chosen to match the observed behaviour of Photoshop's tools:
//! the selected range is affected most, pure black and white stay put for Midtones, and
//! "Protect Tones" works on luminance (scaling RGB so hue and saturation are kept, with gamut
//! compression instead of per-channel clipping).

use serde::{Deserialize, Serialize};

/// Tonal range targeted by Dodge/Burn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ToneRange {
    Shadows,
    #[default]
    Midtones,
    Highlights,
}

/// Rec. 601 luma of a straight RGB triple.
#[inline]
pub fn luma(c: [f32; 3]) -> f32 {
    0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]
}

/// The dodge/burn transfer for one value `v` (0..1) with strength `e` (0..1).
#[inline]
pub fn tone_curve(v: f32, e: f32, range: ToneRange, burn: bool) -> f32 {
    let v = v.max(0.0);
    let e = e.clamp(0.0, 1.0);
    match (range, burn) {
        // Gamma bend, weighted to peak at mid-grey: 0 and 1 are fixed points.
        (ToneRange::Midtones, false) => {
            let w = (4.0 * v * (1.0 - v)).clamp(0.0, 1.0);
            v + w * (v.min(1.0).powf(1.0 / (1.0 + e)) - v)
        }
        (ToneRange::Midtones, true) => {
            let w = (4.0 * v * (1.0 - v)).clamp(0.0, 1.0);
            v + w * (v.min(1.0).powf(1.0 + e) - v)
        }
        // Shadows / highlights: `v + e·w·(target − v)` with weight (1−v)² or v², so the chosen end
        // moves most (black lifts when dodging shadows, white drops when burning highlights).
        (ToneRange::Shadows, false) => v + e * (1.0 - v.min(1.0)).powi(2) * (1.0 - v.min(1.0)),
        (ToneRange::Shadows, true) => v - e * (1.0 - v.min(1.0)).powi(2) * v,
        (ToneRange::Highlights, false) => v + e * v.min(1.0).powi(2) * (1.0 - v.min(1.0)),
        (ToneRange::Highlights, true) => v - e * v.min(1.0).powi(2) * v,
    }
}

/// Bring an RGB triple back into 0..1 while keeping its luma (mix toward grey), instead of clipping
/// each channel (which would shift hue).
#[inline]
fn compress_gamut(c: [f32; 3], l: f32) -> [f32; 3] {
    let l = l.clamp(0.0, 1.0);
    let (lo, hi) = (c[0].min(c[1]).min(c[2]), c[0].max(c[1]).max(c[2]));
    let mut t = 1.0f32;
    if hi > 1.0 && hi - l > 1e-9 {
        t = t.min((1.0 - l) / (hi - l));
    }
    if lo < 0.0 && l - lo > 1e-9 {
        t = t.min(l / (l - lo));
    }
    [l + (c[0] - l) * t, l + (c[1] - l) * t, l + (c[2] - l) * t]
}

/// Dodge (`burn = false`) or Burn one straight-RGB colour with strength `e` (0..1).
///
/// With `protect_tones`, the curve is applied to luma and the colour scaled to the new luma
/// (hue and saturation preserved, gamut-compressed); otherwise each channel goes through the curve.
pub fn dodge_burn(c: [f32; 3], e: f32, range: ToneRange, burn: bool, protect_tones: bool) -> [f32; 3] {
    if e <= 0.0 {
        return c;
    }
    if protect_tones {
        let l = luma(c);
        let nl = tone_curve(l, e, range, burn);
        let out = if l > 1e-6 { [c[0] * nl / l, c[1] * nl / l, c[2] * nl / l] } else { [nl; 3] };
        compress_gamut(out, nl)
    } else {
        [tone_curve(c[0], e, range, burn), tone_curve(c[1], e, range, burn), tone_curve(c[2], e, range, burn)]
    }
}

/// HSV-style saturation (chroma / max) of an RGB triple.
#[inline]
pub fn saturation(c: [f32; 3]) -> f32 {
    let (lo, hi) = (c[0].min(c[1]).min(c[2]), c[0].max(c[1]).max(c[2]));
    if hi <= 1e-6 { 0.0 } else { ((hi - lo) / hi).clamp(0.0, 1.0) }
}

/// Sponge: move a colour away from (`saturate`) or toward its luma grey by `amount` (0..1).
///
/// `vibrance` damps the change where it would clip: saturating affects already-saturated colours
/// less, desaturating affects near-grey colours less (so they never fully collapse to grey in one
/// pass). Saturation is gamut-compressed around luma, never clipped per channel.
pub fn sponge(c: [f32; 3], amount: f32, saturate: bool, vibrance: bool) -> [f32; 3] {
    let mut a = amount.clamp(0.0, 1.0);
    if a <= 0.0 {
        return c;
    }
    let s = saturation(c);
    if vibrance {
        a *= if saturate { 1.0 - s } else { 0.25 + 0.75 * s };
    }
    let l = luma(c);
    let k = if saturate { 1.0 + a } else { 1.0 - a };
    compress_gamut([l + (c[0] - l) * k, l + (c[1] - l) * k, l + (c[2] - l) * k], l)
}

/// Gaussian blur of the window `[x0, x1) × [y0, y1)` of an interleaved `w × h × ch` buffer, reading
/// neighbours with edge clamping. With an alpha channel (`alpha = Some(index)`), colour is blurred
/// premultiplied so transparent pixels don't bleed their (meaningless) colour. Returns the window's
/// blurred pixels (`(x1−x0) × (y1−y0) × ch`).
#[allow(clippy::too_many_arguments)]
pub fn local_blur(data: &[f32], w: usize, h: usize, ch: usize, alpha: Option<usize>, win: (usize, usize, usize, usize), sigma: f32) -> Vec<f32> {
    let (x0, y0, x1, y1) = win;
    let (ow, oh) = (x1 - x0, y1 - y0);
    if ow == 0 || oh == 0 || w == 0 || h == 0 || ch == 0 {
        return Vec::new();
    }
    let r = (sigma * 3.0).ceil().max(1.0) as i32;
    let k: Vec<f32> = (-r..=r).map(|i| (-((i * i) as f32) / (2.0 * sigma * sigma).max(1e-6)).exp()).collect();
    let ks: f32 = k.iter().sum();
    let k: Vec<f32> = k.into_iter().map(|v| v / ks).collect();
    let th = oh + 2 * r as usize;
    // Clamped source offsets, shared by every row.
    let cols: Vec<usize> = (0..ow + 2 * r as usize).map(|sx| (x0 as i32 + sx as i32 - r).clamp(0, w as i32 - 1) as usize * ch).collect();
    let rows: Vec<usize> = (0..th).map(|ty| (y0 as i32 + ty as i32 - r).clamp(0, h as i32 - 1) as usize * w * ch).collect();
    let padded_w = cols.len();
    let a = alpha.unwrap_or(usize::MAX);
    let mut tmp = vec![0.0f32; ow * th * ch];
    let horizontal = |ty: usize, row: &mut [f32], scratch: &mut [f32]| {
        let base = rows[ty];
        for (sx, &col) in cols.iter().enumerate() {
            let s = base + col;
            let px = &data[s..s + ch];
            let o = sx * ch;
            if let Some(a) = alpha {
                let av = px.get(a).copied().unwrap_or(1.0);
                for c in 0..ch {
                    scratch[o + c] = if c == a { px[c] } else { px[c] * av };
                }
            } else {
                scratch[o..o + ch].copy_from_slice(px);
            }
        }

        for (i, kv) in k.iter().enumerate() {
            let src = &scratch[i * ch..(i + ow) * ch];
            for (dst, src) in row.iter_mut().zip(src) {
                *dst += *src * kv;
            }
        }
    };
    // Small dabs stay on one thread: rayon's task overhead would outweigh the convolution.
    #[cfg(not(target_arch = "wasm32"))]
    {
        use rayon::prelude::*;
        if ow * th >= 64 * 64 {
            tmp.par_chunks_mut(ow * ch).enumerate().for_each_init(|| vec![0.0f32; padded_w * ch], |scratch, (ty, row)| horizontal(ty, row, scratch));
        } else {
            let mut scratch = vec![0.0f32; padded_w * ch];
            for (ty, row) in tmp.chunks_mut(ow * ch).enumerate() {
                horizontal(ty, row, &mut scratch);
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let mut scratch = vec![0.0f32; padded_w * ch];
        for (ty, row) in tmp.chunks_mut(ow * ch).enumerate() {
            horizontal(ty, row, &mut scratch);
        }
    }
    let mut out = vec![0.0f32; ow * oh * ch];
    let vertical = |oy: usize, row: &mut [f32]| {
        // Tap-major: each `tmp` row is read once, sequentially. Per pixel the tap order is still
        // 0..k, so this is the same sum as the pixel-major loop, with far fewer cache misses.
        for (i, kv) in k.iter().enumerate() {
            let src = &tmp[(oy + i) * ow * ch..][..ow * ch];
            for (o, s) in row.iter_mut().zip(src) {
                *o += *s * kv;
            }
        }
        if alpha.is_some() {
            let src = ((y0 + oy) * w + x0) * ch;
            for ox in 0..ow {
                let o = ox * ch;
                let av = row[o + a];
                for c in 0..ch {
                    if c != a {
                        row[o + c] = if av > 1e-6 { row[o + c] / av } else { data[src + ox * ch + c] };
                    }
                }
            }
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        use rayon::prelude::*;
        if ow * oh >= 64 * 64 {
            out.par_chunks_mut(ow * ch).enumerate().for_each(|(oy, row)| vertical(oy, row));
        } else {
            for (oy, row) in out.chunks_mut(ow * ch).enumerate() {
                vertical(oy, row);
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    for (oy, row) in out.chunks_mut(ow * ch).enumerate() {
        vertical(oy, row);
    }
    out
}

/// Unsharp-mask sharpen of a window (same layout as [`local_blur`]): `v + amount·(v − blur(v))`.
///
/// With `protect_detail`, each result is clamped to the 3×3 neighbourhood's range, which stops
/// halos and noise from being amplified into new extremes. Alpha is left unchanged.
#[allow(clippy::too_many_arguments)]
pub fn local_sharpen(
    data: &[f32],
    w: usize,
    h: usize,
    ch: usize,
    alpha: Option<usize>,
    win: (usize, usize, usize, usize),
    amount: f32,
    protect_detail: bool,
) -> Vec<f32> {
    let (x0, y0, x1, y1) = win;
    let ow = x1 - x0;
    let blur = local_blur(data, w, h, ch, alpha, win, 1.0);
    let mut out = vec![0.0f32; blur.len()];
    for y in y0..y1 {
        for x in x0..x1 {
            let o = ((y - y0) * ow + (x - x0)) * ch;
            let i = (y * w + x) * ch;
            for c in 0..ch {
                let v = data[i + c];
                if Some(c) == alpha {
                    out[o + c] = v;
                    continue;
                }
                let mut s = v + amount * (v - blur[o + c]);
                if protect_detail {
                    let (mut lo, mut hi) = (v, v);
                    for (dx, dy) in [(-1i32, -1i32), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                        let (nx, ny) = ((x as i32 + dx).clamp(0, w as i32 - 1) as usize, (y as i32 + dy).clamp(0, h as i32 - 1) as usize);
                        let nv = data[(ny * w + nx) * ch + c];
                        lo = lo.min(nv);
                        hi = hi.max(nv);
                    }
                    s = s.clamp(lo, hi);
                }
                out[o + c] = s;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dodge_midtones_affects_midtones_most() {
        let g = |v: f32| dodge_burn([v; 3], 0.5, ToneRange::Midtones, false, true)[0] - v;
        assert!(g(0.5) > 0.1, "{}", g(0.5));
        assert!(g(0.5) > 2.0 * g(0.1));
        assert!(g(0.5) > 2.0 * g(0.95));
        assert_eq!(dodge_burn([0.0; 3], 0.5, ToneRange::Midtones, false, true), [0.0; 3]);
        let b = dodge_burn([0.5; 3], 0.5, ToneRange::Midtones, true, true)[0];
        assert!(b < 0.45);
    }

    #[test]
    fn ranges_target_their_tones() {
        let d = |v: f32, r| tone_curve(v, 0.5, r, false) - v;
        assert!(d(0.1, ToneRange::Shadows) > d(0.9, ToneRange::Shadows));
        assert!(d(0.9, ToneRange::Highlights) > d(0.1, ToneRange::Highlights));
        let b = |v: f32, r| v - tone_curve(v, 0.5, r, true);
        assert!(b(0.9, ToneRange::Highlights) > b(0.2, ToneRange::Highlights));
        assert!(b(0.3, ToneRange::Shadows) / 0.3 > b(0.9, ToneRange::Shadows) / 0.9);
    }

    #[test]
    fn protect_tones_keeps_hue() {
        let c = [0.6, 0.3, 0.2];
        let p = dodge_burn(c, 0.8, ToneRange::Midtones, false, true);
        assert!(p.iter().all(|v| (0.0..=1.0).contains(v)));
        // Ratios (hue) preserved when nothing clips.
        assert!((p[0] / p[1] - c[0] / c[1]).abs() < 0.05, "{p:?}");
    }

    #[test]
    fn sponge_changes_saturation() {
        let c = [0.8, 0.4, 0.3];
        let d = sponge(c, 0.5, false, false);
        assert!(saturation(d) < saturation(c));
        assert!((luma(d) - luma(c)).abs() < 1e-4);
        let s = sponge(c, 0.5, true, false);
        assert!(saturation(s) > saturation(c));
        assert!(s.iter().all(|v| (0.0..=1.0).contains(v)));
        // Vibrance protects already-saturated colours when saturating.
        let v = sponge([1.0, 0.1, 0.1], 0.5, true, true);
        let nv = sponge([1.0, 0.1, 0.1], 0.5, true, false);
        assert!((v[1] - 0.1).abs() <= (nv[1] - 0.1).abs());
        assert_eq!(sponge([0.5; 3], 1.0, false, false), [0.5; 3]);
    }

    #[test]
    fn blur_reduces_variance_and_sharpen_increases_it() {
        let (w, h, ch) = (16, 16, 1);
        let data: Vec<f32> = (0..w * h).map(|i| if (i % w + i / w) % 2 == 0 { 0.2 } else { 0.8 }).collect();
        let var = |v: &[f32]| {
            let m = v.iter().sum::<f32>() / v.len() as f32;
            v.iter().map(|x| (x - m).powi(2)).sum::<f32>() / v.len() as f32
        };
        let win = (4, 4, 12, 12);
        let orig: Vec<f32> = (4..12).flat_map(|y| (4..12).map(move |x| (x, y))).map(|(x, y)| data[y * w + x]).collect();
        let b = local_blur(&data, w, h, ch, None, win, 1.5);
        assert!(var(&b) < var(&orig) * 0.2);
        let smooth: Vec<f32> = (0..w * h).map(|i| 0.3 + 0.03 * (i % w) as f32 + if i % w == 8 { 0.2 } else { 0.0 }).collect();
        let s = local_sharpen(&smooth, w, h, ch, None, win, 1.0, false);
        let so: Vec<f32> = (4..12).flat_map(|y| (4..12).map(move |x| (x, y))).map(|(x, y)| smooth[y * w + x]).collect();
        assert!(var(&s) > var(&so));
        // Protect detail never exceeds the local range.
        let sp = local_sharpen(&smooth, w, h, ch, None, win, 4.0, true);
        let hi = smooth.iter().cloned().fold(0.0, f32::max);
        assert!(sp.iter().all(|v| *v <= hi + 1e-6));
    }

    #[test]
    fn local_blur_matches_direct_convolution() {
        // The optimised kernel (precomputed clamped offsets, parallel rows) must stay bit-identical
        // to the straightforward separable convolution, on both sides of the parallel threshold.
        let (w, h, ch) = (90, 70, 4);
        let data: Vec<f32> = (0..w * h * ch).map(|i| (i * 37 % 101) as f32 / 100.0).collect();
        let sigma = 2.3f32;
        let r = (sigma * 3.0).ceil().max(1.0) as i32;
        let k: Vec<f32> = (-r..=r).map(|i| (-((i * i) as f32) / (2.0 * sigma * sigma)).exp()).collect();
        let ks: f32 = k.iter().sum();
        let k: Vec<f32> = k.into_iter().map(|v| v / ks).collect();
        for alpha in [None, Some(3)] {
            for win in [(20, 20, 40, 40), (5, 5, 85, 65)] {
                let (x0, y0, x1, y1) = win;
                let (ow, oh) = (x1 - x0, y1 - y0);
                let get = |x: i32, y: i32| {
                    let (xx, yy) = (x.clamp(0, w as i32 - 1) as usize, y.clamp(0, h as i32 - 1) as usize);
                    let i = (yy * w + xx) * ch;
                    let mut p = data[i..i + ch].to_vec();
                    if let Some(a) = alpha {
                        let av = p[a];
                        for (c, v) in p.iter_mut().enumerate() {
                            if c != a {
                                *v *= av;
                            }
                        }
                    }
                    p
                };
                let th = oh + 2 * r as usize;
                let mut tmp = vec![0.0f32; ow * th * ch];
                for ty in 0..th {
                    let y = y0 as i32 + ty as i32 - r;
                    for ox in 0..ow {
                        let o = (ty * ow + ox) * ch;
                        for (i, kv) in k.iter().enumerate() {
                            let p = get(x0 as i32 + ox as i32 + i as i32 - r, y);
                            for c in 0..ch {
                                tmp[o + c] += p[c] * kv;
                            }
                        }
                    }
                }
                let mut out = vec![0.0f32; ow * oh * ch];
                for oy in 0..oh {
                    for ox in 0..ow {
                        let o = (oy * ow + ox) * ch;
                        for (i, kv) in k.iter().enumerate() {
                            let s = ((oy + i) * ow + ox) * ch;
                            for c in 0..ch {
                                out[o + c] += tmp[s + c] * kv;
                            }
                        }
                        if let Some(a) = alpha {
                            let av = out[o + a];
                            let src = ((y0 + oy) * w + x0 + ox) * ch;
                            for c in 0..ch {
                                if c != a {
                                    out[o + c] = if av > 1e-6 { out[o + c] / av } else { data[src + c] };
                                }
                            }
                        }
                    }
                }
                assert_eq!(local_blur(&data, w, h, ch, alpha, win, sigma), out);
            }
        }
    }

    #[test]
    fn local_blur_empty_window_is_empty() {
        assert!(local_blur(&[], 0, 0, 4, Some(3), (0, 0, 0, 0), 1.0).is_empty());
    }

    #[test]
    fn blur_ignores_transparent_colour() {
        // Red opaque next to transparent green: blurred colour stays red.
        let (w, h, ch) = (8, 1, 4);
        let data: Vec<f32> = (0..w).flat_map(|x| if x < 4 { [1.0, 0.0, 0.0, 1.0] } else { [0.0, 1.0, 0.0, 0.0] }).collect();
        let b = local_blur(&data, w, h, ch, Some(3), (2, 0, 6, 1), 1.0);
        for p in b.as_chunks::<4>().0 {
            if p[3] > 0.01 {
                assert!(p[0] > 0.99 && p[1] < 0.01, "{p:?}");
            }
        }
    }
}
