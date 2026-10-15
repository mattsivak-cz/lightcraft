//! Focus stack sharpness at a real frame size, against an analytic scene: texture made of waves
//! (up to 0.33 cycles per pixel) whose defocus is exact (a Gaussian blur of a wave only lowers its
//! amplitude, by exp(−2π²σ²f²)). Depth runs left (near) to right (far); frame k is focused at depth
//! k/(n−1), blurred up to 6 px away from it, magnified a little more than the last (focus breathing)
//! and noisy. The texture's contrast falls from top to bottom in three bands, so subtle texture lost to
//! averaging shows on its own.
//!
//! Measured by projecting the result's fine detail onto the scene's ([`detail_gain`]; noise doesn't count), against
//! the same for an ideal stack (each pixel from its best-focused frame, noiseless). The small versions the alignment
//! runs on are 8× below the frames, as a 1200 px one is below a 61 MP raw. Before the full-size refinement and the
//! Lanczos-3 warp: corner errors up to 1.9 px, 56 / 72 / 73 % of the ideal's texture; now 0.03 px, 98–99 %.

use lightcraft_merge::focus::{FocusOptions, Load, stack_focus};
use lightcraft_merge::frame::Frame;
use lightcraft_merge::metrics::detail_gain;
use lightcraft_merge::{MergeError, no_progress};
use lightcraft_raster::Rgb32f;

mod common;
use common::Rng;
use lightcraft_raster::resample::{Filter, fit};

const W: usize = 3200;
const H: usize = 2000;
const N: usize = 9;
/// Magnification step between frames.
const BREATHING: f64 = 0.004;
/// Blur (σ, px) a whole depth range away from the focus: neighbouring frames (1/8 of the depth
/// apart) overlap, the farthest point from any focus is blurred σ = 0.375 px.
const MAX_SIGMA: f64 = 6.0;

/// Wave j: (frequency in cycles per pixel, direction, phase).
fn waves() -> Vec<(f64, f64, f64)> {
    (0..14).map(|j| (0.02 + 0.0238 * j as f64, j as f64 * 2.399_963, j as f64 * 1.7)).collect()
}

/// Contrast of the texture at scene row y: high, mid, low thirds.
fn contrast(y: f64) -> f64 {
    if y < H as f64 / 3.0 {
        0.02
    } else if y < 2.0 * H as f64 / 3.0 {
        0.007
    } else {
        0.0025
    }
}

/// Side of the cells that each hold one blob.
const CELL: f64 = 48.0;

/// The blob of cell (i, j): centre, size (σ, px) and amplitude (relative to the contrast), from a hash.
fn blob(i: i64, j: i64) -> (f64, f64, f64, f64) {
    let mut h = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (j as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
    let mut u = || {
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        (h >> 11) as f64 / (1u64 << 53) as f64
    };
    let (cx, cy) = ((i as f64 + u()) * CELL, (j as f64 + u()) * CELL);
    (cx, cy, 1.5 + 10.0 * u(), (u() - 0.5) * 6.0)
}

/// The scene at scene point (x, y) seen with a defocus of σ px: waves (fine texture) and Gaussian blobs (a pattern
/// that doesn't repeat, for the alignment to lock on). Defocus is exact for both: a wave's amplitude falls by
/// exp(−2π²σ²f²), a blob of size s becomes one of size √(s² + σ²) with the same volume.
fn scene(x: f64, y: f64, sigma: f64, w: &[(f64, f64, f64)]) -> f32 {
    let a = contrast(y);
    let mut v = 0.25;
    let (ci, cj) = ((x / CELL).floor() as i64, (y / CELL).floor() as i64);
    for j in cj - 1..=cj + 1 {
        for i in ci - 1..=ci + 1 {
            let (bx, by, bs, ba) = blob(i, j);
            let s2 = bs * bs + sigma * sigma;
            let d2 = (x - bx).powi(2) + (y - by).powi(2);
            v += a * ba * (bs * bs / s2) * (-d2 / (2.0 * s2)).exp();
        }
    }
    for &(f, d, ph) in w {
        let g = (-2.0 * std::f64::consts::PI.powi(2) * sigma * sigma * f * f).exp();
        v += a * g * ((x * d.cos() + y * d.sin()) * f * std::f64::consts::TAU + ph).sin();
    }
    v.max(0.0) as f32
}

fn zoom(k: usize) -> f64 {
    1.0 + BREATHING * k as f64
}

/// Frame pixel → scene point for magnification `z`.
fn to_scene(x: f64, y: f64, z: f64) -> (f64, f64) {
    let (cx, cy) = (W as f64 / 2.0, H as f64 / 2.0);
    (cx + (x - cx) / z, cy + (y - cy) / z)
}

fn frame(k: usize, w: &[(f64, f64, f64)]) -> Rgb32f {
    let focus = k as f64 / (N - 1) as f64;
    let mut rng = Rng(k as u64 * 0x9e37_79b9 + 5);
    let mut img = Rgb32f::from_fn(W, H, |x, y| {
        let (sx, sy) = to_scene(x as f64 + 0.5, y as f64 + 0.5, zoom(k));
        let sigma = ((sx / W as f64).clamp(0.0, 1.0) - focus).abs() * MAX_SIGMA;
        [scene(sx, sy, sigma, w); 3]
    });
    for p in img.data.iter_mut() {
        let s = p[0];
        let v = (s + (2e-5 * s + 2e-8).sqrt() * rng.gauss()).clamp(0.0, 1.0);
        *p = [v; 3];
    }
    img
}

#[test]
fn focus_stack_keeps_the_texture() {
    let w = waves();
    let frames: Vec<Rgb32f> = (0..N).map(|k| frame(k, &w)).collect();
    // small versions as far below the frame as a 1200 px one is below a 61 MP raw (≈ 8×)
    let load = |i: usize, what: Load| -> Result<Frame, MergeError> {
        let img = if what == Load::Align { fit(&frames[i], W / 8, W / 8, Filter::Box) } else { frames[i].clone() };
        Ok(Frame::from_linear_rec2020(img, None))
    };
    let t0 = std::time::Instant::now();
    let r = stack_focus(N, &load, &FocusOptions::default(), &no_progress).unwrap();
    let secs = t0.elapsed().as_secs_f64();
    // the all-sharp scene in the reference's geometry
    let zr = zoom(r.reference);
    let truth = Rgb32f::from_fn(W, H, |x, y| {
        let (sx, sy) = to_scene(x as f64 + 0.5, y as f64 + 0.5, zr);
        [scene(sx, sy, 0.0, &w); 3]
    });
    // the best a stack can do: each pixel from the frame focused nearest to it, without noise
    let ideal = Rgb32f::from_fn(W, H, |x, y| {
        let (sx, sy) = to_scene(x as f64 + 0.5, y as f64 + 0.5, zr);
        let z = (sx / W as f64).clamp(0.0, 1.0);
        let near = (0..N).map(|k| (z - k as f64 / (N - 1) as f64).abs()).fold(f64::INFINITY, f64::min);
        [scene(sx, sy, near * MAX_SIGMA, &w); 3]
    });
    // alignment against the true magnifications: frame k sees reference pixel p at c + (p − c)·zₖ/z_ref
    let (cx, cy) = (W as f64 / 2.0, H as f64 / 2.0);
    let corner: Vec<f64> = (0..N)
        .map(|k| {
            let s = zoom(k) / zr;
            [(0.0, 0.0), (W as f64, 0.0), (0.0, H as f64), (W as f64, H as f64)]
                .iter()
                .map(|&(x, y)| {
                    let p = r.alignments[k].h.apply(lightcraft_geom::Point::new(x, y));
                    (p.x - (cx + (x - cx) * s)).hypot(p.y - (cy + (y - cy) * s))
                })
                .fold(0.0, f64::max)
        })
        .collect();
    let m = 60;
    let band = |b: usize| {
        let (y0, y1) = (b * H / 3 + m, (b + 1) * H / 3 - m);
        let keep = |x: usize, y: usize| x > m && x < W - m && y > y0 && y < y1;
        detail_gain(&r.image, &truth, keep).unwrap() / detail_gain(&ideal, &truth, keep).unwrap()
    };
    let gains = [band(0), band(1), band(2)];
    eprintln!("texture kept of the ideal stack's (high / mid / low contrast) {gains:.3?}, reference {}, {secs:.1} s", r.reference);
    eprintln!("alignment corner error {corner:.2?} px");
    assert!(corner.iter().all(|e| *e < 0.15), "alignment corner error {corner:?} px");
    assert!(gains.iter().all(|g| *g > 0.95), "texture kept {gains:?}");
}
