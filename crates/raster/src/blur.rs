//! Blurs: separable box passes (exact running sums) approximating Gaussians, plus directional
//! and radial (spin/zoom) blurs.
//!
//! Box passes run straight on row-major pixels: horizontal passes per row, vertical passes over
//! blocks of rows with a per-column running sum (no transposes), ping-ponging between two
//! buffers so a Gaussian allocates two images regardless of the number of passes.

use rayon::prelude::*;

use crate::{Image, Px};

/// One horizontal box pass of radius `r > 0` (window 2r+1) over `row` into `o` (same length);
/// `repeat` = edge pixels extend, otherwise transparent.
#[inline]
fn box_row(row: &[Px], o: &mut [Px], r: usize, repeat: bool) {
    let w = row.len();
    let norm = 1.0 / (2 * r + 1) as f32;
    let get = |i: isize| -> Px {
        if i < 0 {
            if repeat { row[0] } else { [0.0; 4] }
        } else if i as usize >= w {
            if repeat { row[w - 1] } else { [0.0; 4] }
        } else {
            row[i as usize]
        }
    };
    let mut acc = [0.0f32; 4];
    for i in -(r as isize)..=(r as isize) {
        let p = get(i);
        for c in 0..4 {
            acc[c] += p[c];
        }
    }
    for x in 0..w {
        o[x] = acc.map(|v| v * norm);
        let add = get(x as isize + r as isize + 1);
        let sub = get(x as isize - r as isize);
        for c in 0..4 {
            acc[c] += add[c] - sub[c];
        }
    }
}

/// Horizontal box passes of radii `radii` (each > 0, at least one), one after the other, over
/// each row of `src` (width `w`) into `out`. All of a row's passes run while the row is in the
/// cache, between two row-sized scratch buffers, so the image is read and written once however
/// many passes there are (each pass computes what a whole-image pass would).
fn box_h_into(src: &[Px], out: &mut [Px], w: usize, radii: &[usize], repeat: bool) {
    let Some((&last, first)) = radii.split_last() else {
        out.copy_from_slice(src);
        return;
    };
    out.par_chunks_mut(w).zip(src.par_chunks(w)).for_each_init(
        || (vec![[0.0f32; 4]; if first.is_empty() { 0 } else { w }], vec![[0.0f32; 4]; if first.len() > 1 { w } else { 0 }]),
        |(a, b), (o, row)| {
            let n = row.len();
            if first.is_empty() {
                box_row(row, o, last, repeat);
                return;
            }
            let Some(mut a) = a.get_mut(..n) else { return };
            let mut b = b.get_mut(..n).unwrap_or_default();
            // `a` holds the latest pass, `b` the one before.
            for (i, &r) in first.iter().enumerate() {
                if i == 0 {
                    box_row(row, a, r, repeat);
                } else {
                    std::mem::swap(&mut a, &mut b);
                    box_row(b, a, r, repeat);
                }
            }
            box_row(a, o, last, repeat);
        },
    );
}

/// One vertical box pass (same semantics as [`box_h_into`] along columns).
fn box_v_into(src: &[Px], out: &mut [Px], w: usize, h: usize, r: usize, repeat: bool) {
    if r == 0 {
        out.copy_from_slice(src);
        return;
    }
    let norm = 1.0 / (2 * r + 1) as f32;
    let block = (4 * r).clamp(32, 256);
    let row = |i: isize| -> Option<&[Px]> {
        if i < 0 {
            repeat.then(|| &src[0..w])
        } else if i as usize >= h {
            repeat.then(|| &src[(h - 1) * w..h * w])
        } else {
            Some(&src[i as usize * w..(i as usize + 1) * w])
        }
    };
    out.par_chunks_mut(w * block).enumerate().for_each(|(bi, chunk)| {
        let ya = (bi * block) as isize;
        let mut acc = vec![[0.0f32; 4]; w];
        for i in ya - r as isize..=ya + r as isize {
            if let Some(rw) = row(i) {
                for (a, p) in acc.iter_mut().zip(rw) {
                    for c in 0..4 {
                        a[c] += p[c];
                    }
                }
            }
        }
        for (yy, o) in chunk.chunks_mut(w).enumerate() {
            let y = ya + yy as isize;
            for (o, a) in o.iter_mut().zip(&acc) {
                *o = a.map(|v| v * norm);
            }
            match (row(y + r as isize + 1), row(y - r as isize)) {
                (Some(add), Some(sub)) => {
                    for ((a, p), q) in acc.iter_mut().zip(add).zip(sub) {
                        for c in 0..4 {
                            a[c] += p[c] - q[c];
                        }
                    }
                }
                (Some(add), None) => {
                    for (a, p) in acc.iter_mut().zip(add) {
                        for c in 0..4 {
                            a[c] += p[c];
                        }
                    }
                }
                (None, Some(sub)) => {
                    for (a, q) in acc.iter_mut().zip(sub) {
                        for c in 0..4 {
                            a[c] -= q[c];
                        }
                    }
                }
                (None, None) => {}
            }
        }
    });
}

/// Run horizontal passes with radii `rx` then vertical passes with radii `ry`.
fn box_passes(img: &Image, rx: &[usize], ry: &[usize], repeat: bool) -> Image {
    let (w, h) = (img.width as usize, img.height as usize);
    let (h_radii, v_radii) = radii(rx, ry);
    if (h_radii.is_empty() && v_radii.is_empty()) || w == 0 || h == 0 {
        return img.clone();
    }
    let (cur, done) = first_stage(&img.data, w, h, &h_radii, &v_radii, repeat);
    let data = v_stages(cur, None, w, h, v_radii.get(done..).unwrap_or_default(), repeat);
    Image { width: img.width, height: img.height, data }
}

/// [`box_passes`] on an image the caller is done with: once the first stage has read it, its
/// buffer takes the later passes, so a blur holds two images at its peak instead of three.
fn box_passes_owned(mut img: Image, rx: &[usize], ry: &[usize], repeat: bool) -> Image {
    let (w, h) = (img.width as usize, img.height as usize);
    let (h_radii, v_radii) = radii(rx, ry);
    if (h_radii.is_empty() && v_radii.is_empty()) || w == 0 || h == 0 {
        return img;
    }
    let src = std::mem::take(&mut img.data);
    let (cur, done) = first_stage(&src, w, h, &h_radii, &v_radii, repeat);
    img.data = v_stages(cur, Some(src), w, h, v_radii.get(done..).unwrap_or_default(), repeat);
    img
}

/// The horizontal and vertical radii that blur at all.
fn radii(rx: &[usize], ry: &[usize]) -> (Vec<usize>, Vec<usize>) {
    let keep = |r: &[usize]| r.iter().copied().filter(|&r| r > 0).collect::<Vec<_>>();
    (keep(rx), keep(ry))
}

/// The first stage of a blur of `src` into a new buffer: all the horizontal passes
/// ([`box_h_into`]), or without any the first vertical pass. Returns the buffer and how many
/// vertical passes are done.
fn first_stage(src: &[Px], w: usize, h: usize, h_radii: &[usize], v_radii: &[usize], repeat: bool) -> (Vec<Px>, usize) {
    let mut out = vec![[0.0; 4]; src.len()];
    if !h_radii.is_empty() {
        box_h_into(src, &mut out, w, h_radii, repeat);
        return (out, 0);
    }
    match v_radii.first() {
        Some(&r) => {
            box_v_into(src, &mut out, w, h, r, repeat);
            (out, 1)
        }
        None => {
            out.copy_from_slice(src);
            (out, 0)
        }
    }
}

/// Vertical passes of `radii` over `cur`, ping-ponging with `spare` (a buffer of the same
/// length, allocated when a pass needs one and none is given).
fn v_stages(mut cur: Vec<Px>, mut spare: Option<Vec<Px>>, w: usize, h: usize, radii: &[usize], repeat: bool) -> Vec<Px> {
    for &r in radii {
        let out = spare.get_or_insert_with(|| vec![[0.0; 4]; cur.len()]);
        box_v_into(&cur, out, w, h, r, repeat);
        std::mem::swap(&mut cur, out);
    }
    cur
}

/// Box radii for `n` passes approximating a Gaussian of `sigma` (Kovesi / "boxes for Gauss").
fn box_radii(sigma: f64, n: usize) -> Vec<usize> {
    if sigma <= 0.0 {
        return vec![0; n];
    }
    let w_ideal = (12.0 * sigma * sigma / n as f64 + 1.0).sqrt();
    let mut wl = w_ideal.floor() as i64;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wu = wl + 2;
    let m_ideal = (12.0 * sigma * sigma - (n as i64 * wl * wl) as f64 - 4.0 * n as f64 * wl as f64 - 3.0 * n as f64) / (-4.0 * wl as f64 - 4.0);
    let m = m_ideal.round() as i64;
    (0..n as i64).map(|i| (((if i < m { wl } else { wu }) - 1) / 2).max(0) as usize).collect()
}

/// Separable box blur with given radii, `iterations` passes.
pub fn box_blur(img: &Image, rx: usize, ry: usize, iterations: usize, repeat: bool) -> Image {
    let n = iterations.max(1);
    box_passes(img, &vec![rx; n], &vec![ry; n], repeat)
}

/// [`box_blur`] of an image the caller is done with (its buffer is reused).
pub fn box_blur_owned(img: Image, rx: usize, ry: usize, iterations: usize, repeat: bool) -> Image {
    let n = iterations.max(1);
    box_passes_owned(img, &vec![rx; n], &vec![ry; n], repeat)
}

/// Gaussian blur with standard deviations in pixels (3 box passes per axis).
pub fn gaussian_blur(img: &Image, sigma_x: f64, sigma_y: f64, repeat: bool) -> Image {
    let rx = if sigma_x > 0.05 { box_radii(sigma_x, 3) } else { vec![] };
    let ry = if sigma_y > 0.05 { box_radii(sigma_y, 3) } else { vec![] };
    box_passes(img, &rx, &ry, repeat)
}

/// [`gaussian_blur`] of an image the caller is done with (its buffer is reused).
pub fn gaussian_blur_owned(img: Image, sigma_x: f64, sigma_y: f64, repeat: bool) -> Image {
    let rx = if sigma_x > 0.05 { box_radii(sigma_x, 3) } else { vec![] };
    let ry = if sigma_y > 0.05 { box_radii(sigma_y, 3) } else { vec![] };
    box_passes_owned(img, &rx, &ry, repeat)
}

/// Motion blur along `angle_deg` (0 = vertical in AE's Directional Blur, measured clockwise from
/// up) over `length` pixels.
pub fn directional_blur(img: &Image, angle_deg: f64, length: f64) -> Image {
    if length < 0.5 {
        return img.clone();
    }
    let a = angle_deg.to_radians();
    let (dx, dy) = (a.sin(), -a.cos());
    let n = (length.ceil() as usize * 2 + 1).clamp(3, 257);
    // Along an image axis every pixel takes the same taps, so the samples fold into one 1-D kernel.
    if dx.abs() < 1e-9 || dy.abs() < 1e-9 {
        let d = if dy.abs() < 1e-9 { dx.signum() } else { dy.signum() };
        return axis_blur(img, &axis_kernel(d, length, n), dy.abs() < 1e-9);
    }
    let mut out = Image::new(img.width, img.height);
    out.rows_mut().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            let mut acc = [0.0f32; 4];
            for i in 0..n {
                let t = (i as f64 / (n - 1) as f64 - 0.5) * length;
                let p = img.sample_bilinear(x as f64 + 0.5 + dx * t, y as f64 + 0.5 + dy * t);
                for c in 0..4 {
                    acc[c] += p[c];
                }
            }
            *px = acc.map(|v| v / n as f32);
        }
    });
    out
}

/// The taps of [`directional_blur`] along one axis as `(first offset, weights)`: each bilinear
/// sample splits between the two pixels it falls between.
fn axis_kernel(d: f64, length: f64, n: usize) -> (i64, Vec<f32>) {
    let reach = (length * 0.5).ceil() as i64 + 1;
    let mut k = vec![0.0f64; (2 * reach + 1) as usize];
    for i in 0..n {
        let t = d * (i as f64 / (n - 1) as f64 - 0.5) * length;
        let f = t.floor();
        let (i0, frac) = ((f as i64 + reach) as usize, t - f);
        if let Some(w) = k.get_mut(i0) {
            *w += 1.0 - frac;
        }
        if let Some(w) = k.get_mut(i0 + 1) {
            *w += frac;
        }
    }
    (-reach, k.into_iter().map(|w| (w / n as f64) as f32).collect())
}

/// Convolves the rows (`horizontal`) or columns with `kernel`; transparent outside the image.
fn axis_blur(img: &Image, kernel: &(i64, Vec<f32>), horizontal: bool) -> Image {
    let (first, weights) = kernel;
    let (w, h) = (img.width as i64, img.height as i64);
    let mut out = Image::new(img.width, img.height);
    let stride = img.width.max(1) as usize;
    out.rows_mut().for_each(|(y, row)| {
        if horizontal {
            let Some(src) = img.data.get(y * stride..(y + 1) * stride) else { return };
            for (x, px) in row.iter_mut().enumerate() {
                // Kernel index j reads source column x + first + j.
                let j0 = (-(x as i64 + first)).clamp(0, weights.len() as i64) as usize;
                let j1 = (w - (x as i64 + first)).clamp(0, weights.len() as i64) as usize;
                let s0 = (x as i64 + first + j0 as i64).max(0) as usize;
                let mut acc = [0.0f32; 4];
                if let (Some(ws), Some(ps)) = (weights.get(j0..j1), src.get(s0..)) {
                    for (wt, p) in ws.iter().zip(ps) {
                        for c in 0..4 {
                            acc[c] += p[c] * wt;
                        }
                    }
                }
                *px = acc;
            }
        } else {
            for (j, wt) in weights.iter().enumerate() {
                let sy = y as i64 + first + j as i64;
                if sy < 0 || sy >= h || *wt == 0.0 {
                    continue;
                }
                let Some(src) = img.data.get(sy as usize * stride..(sy as usize + 1) * stride) else { continue };
                for (px, p) in row.iter_mut().zip(src) {
                    for c in 0..4 {
                        px[c] += p[c] * wt;
                    }
                }
            }
        }
    });
    out
}

/// Radial blur around `center`: `spin` (degrees) or zoom (`amount` as a fraction of distance).
pub fn radial_blur(img: &Image, center: (f64, f64), amount: f64, zoom: bool) -> Image {
    if amount.abs() < 1e-6 {
        return img.clone();
    }
    let n = 32usize;
    let mut out = Image::new(img.width, img.height);
    out.rows_mut().for_each(|(y, row)| {
        for (x, px) in row.iter_mut().enumerate() {
            let (vx, vy) = (x as f64 + 0.5 - center.0, y as f64 + 0.5 - center.1);
            let mut acc = [0.0f32; 4];
            for i in 0..n {
                let t = i as f64 / (n - 1) as f64 - 0.5;
                let (sx, sy) = if zoom {
                    let k = 1.0 + t * amount;
                    (center.0 + vx * k, center.1 + vy * k)
                } else {
                    let (s, c) = (t * amount).to_radians().sin_cos();
                    (center.0 + vx * c - vy * s, center.1 + vx * s + vy * c)
                };
                let p = img.sample_bilinear(sx, sy);
                for ch in 0..4 {
                    acc[ch] += p[ch];
                }
            }
            *px = acc.map(|v| v / n as f32);
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference sampling loop the axis-aligned path replaces.
    fn directional_by_samples(img: &Image, dx: f64, dy: f64, length: f64) -> Image {
        let n = (length.ceil() as usize * 2 + 1).clamp(3, 257);
        let mut out = Image::new(img.width, img.height);
        for y in 0..img.height {
            for x in 0..img.width {
                let mut acc = [0.0f32; 4];
                for i in 0..n {
                    let t = (i as f64 / (n - 1) as f64 - 0.5) * length;
                    let p = img.sample_bilinear(x as f64 + 0.5 + dx * t, y as f64 + 0.5 + dy * t);
                    for c in 0..4 {
                        acc[c] += p[c];
                    }
                }
                out.set(x, y, acc.map(|v| v / n as f32));
            }
        }
        out
    }

    #[test]
    fn axis_aligned_directional_blur_matches_the_sampled_one() {
        let mut img = Image::new(37, 23);
        for (i, p) in img.data.iter_mut().enumerate() {
            let v = ((i * 7919) % 101) as f32 / 100.0;
            *p = [v, 1.0 - v, v * 0.5, 0.25 + v * 0.5];
        }
        for (angle, dx, dy) in [(0.0, 0.0, -1.0), (90.0, 1.0, 0.0), (180.0, 0.0, 1.0), (270.0, -1.0, 0.0)] {
            for length in [0.7, 3.0, 9.4, 158.0] {
                let (fast, slow) = (directional_blur(&img, angle, length), directional_by_samples(&img, dx, dy, length));
                for (a, b) in fast.data.iter().zip(&slow.data) {
                    for c in 0..4 {
                        assert!((a[c] - b[c]).abs() < 1e-4, "angle {angle} length {length}: {a:?} vs {b:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn gaussian_preserves_mass() {
        let mut img = Image::new(64, 64);
        img.set(32, 32, [1.0; 4]);
        let b = gaussian_blur(&img, 4.0, 4.0, false);
        let sum: f32 = b.data.iter().map(|p| p[3]).sum();
        assert!((sum - 1.0).abs() < 1e-3, "{sum}");
        assert!(b.get(32, 32)[3] < 0.05);
    }

    fn noisy(w: u32, h: u32) -> Image {
        let mut img = Image::new(w, h);
        for (i, p) in img.data.iter_mut().enumerate() {
            let n = |k: u32| crate::hash_noise(i as u32, k, 7);
            *p = [n(0), n(1), n(2), n(3)];
        }
        img
    }

    /// Horizontal passes run together per row give, bit for bit, what whole-image passes one
    /// after the other give; and a blur that reuses its input's buffer is the borrowing one.
    #[test]
    fn fused_and_buffer_reusing_passes_match_separate_ones() {
        let img = noisy(37, 23);
        let (w, h) = (37usize, 23usize);
        for repeat in [false, true] {
            for radii in [&[2usize][..], &[3, 1], &[4, 4, 5], &[1, 7, 2, 3]] {
                let mut fused = vec![[0.0; 4]; w * h];
                box_h_into(&img.data, &mut fused, w, radii, repeat);
                let mut cur = img.data.clone();
                for &r in radii {
                    let mut next = vec![[0.0; 4]; w * h];
                    box_h_into(&cur, &mut next, w, &[r], repeat);
                    cur = next;
                }
                assert_eq!(fused, cur, "radii {radii:?} repeat {repeat}");
            }
            for (sx, sy) in [(2.5, 2.5), (3.0, 0.0), (0.0, 1.7), (0.0, 0.0), (6.0, 0.3)] {
                assert_eq!(gaussian_blur_owned(img.clone(), sx, sy, repeat), gaussian_blur(&img, sx, sy, repeat), "sigma {sx} {sy} repeat {repeat}");
            }
            for (rx, ry, n) in [(2, 3, 1), (0, 2, 3), (4, 0, 2), (0, 0, 2), (1, 1, 4)] {
                assert_eq!(box_blur_owned(img.clone(), rx, ry, n, repeat), box_blur(&img, rx, ry, n, repeat), "box {rx} {ry} x{n} repeat {repeat}");
            }
        }
    }

    #[test]
    fn flat_image_stays_flat_with_repeat() {
        let img = Image::filled(20, 10, [0.5, 0.5, 0.5, 1.0]);
        let b = gaussian_blur(&img, 3.0, 3.0, true);
        assert!(b.data.iter().all(|p| (p[0] - 0.5).abs() < 1e-5));
    }

    /// Direct (windowed sum) box filter along x or y: the definition the passes must match.
    fn box_ref(img: &Image, r: usize, vertical: bool, repeat: bool) -> Image {
        let mut out = Image::new(img.width, img.height);
        let (w, h) = (img.width as i64, img.height as i64);
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0.0f64; 4];
                for k in -(r as i64)..=(r as i64) {
                    let (sx, sy) = if vertical { (x, y + k) } else { (x + k, y) };
                    let p = if repeat { img.get_clamped(sx, sy) } else { img.get(sx, sy) };
                    for c in 0..4 {
                        acc[c] += p[c] as f64;
                    }
                }
                out.set(x as u32, y as u32, acc.map(|v| (v / (2 * r + 1) as f64) as f32));
            }
        }
        out
    }

    #[test]
    fn passes_match_direct_box_filter() {
        let mut img = Image::new(53, 301);
        for y in 0..301 {
            for x in 0..53 {
                let v = crate::hash_noise(x, y, 3);
                img.set(x, y, [v * 0.5, v * 0.25, v, v]);
            }
        }
        for repeat in [false, true] {
            for (rx, ry) in [(0, 3), (2, 0), (5, 9), (40, 70), (1, 200)] {
                let fast = box_passes(&img, &[rx, rx + 1], &[ry, ry / 2], repeat);
                let mut slow = img.clone();
                for r in [rx, rx + 1] {
                    slow = box_ref(&slow, r, false, repeat);
                }
                for r in [ry, ry / 2] {
                    slow = box_ref(&slow, r, true, repeat);
                }
                for (i, (p, q)) in fast.data.iter().zip(&slow.data).enumerate() {
                    for c in 0..4 {
                        assert!((p[c] - q[c]).abs() < 1e-4, "r {rx},{ry} repeat {repeat} px {i}: {p:?} vs {q:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn radii_sane() {
        let r = box_radii(10.0, 3);
        assert_eq!(r.len(), 3);
        assert!(r.iter().all(|&v| (7..=10).contains(&v)), "{r:?}");
    }
}
