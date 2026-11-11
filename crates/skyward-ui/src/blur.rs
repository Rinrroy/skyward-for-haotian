//! A true Gaussian blur of the page behind the power menu.
//!
//! The panel is some 1440 x 3200 pixels; convolving that twice would keep
//! the CPU busy long enough to see. The page is first boxed down to a
//! quarter of each side, the separable Gaussian kernel is run over that
//! small image (a quarter of the sigma, a sixteenth of the work), and the
//! result is stretched back with bilinear sampling. The blur itself is the
//! real thing: a normalised `exp(-x^2 / 2 sigma^2)` kernel applied in both
//! directions, not a stack of translucent copies of the picture and not a
//! plain box average.

use crate::canvas::Canvas;
use crate::color::Rgba;

/// The blur's radius, in full-resolution pixels.
pub const SIGMA: f32 = 40.0;
/// How much smaller the convolved image is on each axis.
const SCALE: u32 = 4;

type Rgb = [u8; 3];

/// Blur `src`, which holds all of itself, into a whole opaque canvas of the
/// same size.
pub fn gaussian(src: &Canvas) -> Canvas {
    assert!(src.whole(), "a blur reads a canvas that holds all of itself");
    let (sw, sh) = (src.w.div_ceil(SCALE), src.h.div_ceil(SCALE));
    let small = downsample(src, sw, sh);
    let spread = convolve(&small, sw, sh, SIGMA / SCALE as f32);
    upscale(&spread, sw, sh, src.w, src.h)
}

/// Average each `SCALE` x `SCALE` block into one pixel. This is a downsample,
/// not the blur: the Gaussian follows on the small image.
fn downsample(src: &Canvas, w: u32, h: u32) -> Vec<Rgb> {
    let mut out = vec![[0u8; 3]; (w * h) as usize];
    for sy in 0..h {
        for sx in 0..w {
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for dy in 0..SCALE {
                let y = sy * SCALE + dy;
                if y >= src.h {
                    continue;
                }
                for dx in 0..SCALE {
                    let x = sx * SCALE + dx;
                    if x >= src.w {
                        continue;
                    }
                    let at = ((y * src.w + x) * 4) as usize;
                    r += src.data[at] as u32;
                    g += src.data[at + 1] as u32;
                    b += src.data[at + 2] as u32;
                    n += 1;
                }
            }
            if n > 0 {
                out[(sy * w + sx) as usize] = [(r / n) as u8, (g / n) as u8, (b / n) as u8];
            }
        }
    }
    out
}

/// A normalised Gaussian kernel and its radius, three sigmas either side.
fn kernel(sigma: f32) -> (Vec<f32>, i32) {
    let radius = (sigma * 3.0).ceil().max(1.0) as i32;
    let mut k: Vec<f32> = (-radius..=radius).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let sum: f32 = k.iter().sum();
    for v in &mut k {
        *v /= sum;
    }
    (k, radius)
}

/// Two passes of the real kernel, horizontal then vertical, with the edge
/// held by clamping rather than wrapping or fading to black.
fn convolve(src: &[Rgb], w: u32, h: u32, sigma: f32) -> Vec<Rgb> {
    let (k, radius) = kernel(sigma);
    let at = |x: i32, y: i32| -> Rgb {
        let x = x.clamp(0, w as i32 - 1) as u32;
        let y = y.clamp(0, h as i32 - 1) as u32;
        src[(y * w + x) as usize]
    };
    let mut horizontal = vec![[0f32; 3]; (w * h) as usize];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let mut sum = [0f32; 3];
            for (i, weight) in k.iter().enumerate() {
                let p = at(x + i as i32 - radius, y);
                for (s, v) in sum.iter_mut().zip(p) {
                    *s += v as f32 * weight;
                }
            }
            horizontal[(y as u32 * w + x as u32) as usize] = sum;
        }
    }
    let mut out = vec![[0u8; 3]; (w * h) as usize];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let mut sum = [0f32; 3];
            for (i, weight) in k.iter().enumerate() {
                let row = (y + i as i32 - radius).clamp(0, h as i32 - 1) as u32;
                let p = horizontal[(row * w + x as u32) as usize];
                for (s, v) in sum.iter_mut().zip(p) {
                    *s += v * weight;
                }
            }
            out[(y as u32 * w + x as u32) as usize] = sum.map(|v| v.round().clamp(0.0, 255.0) as u8);
        }
    }
    out
}

/// Stretch the small blurred image back to the panel, sampling between its
/// pixels so the upsampling adds no hard edges of its own.
fn upscale(small: &[Rgb], sw: u32, sh: u32, w: u32, h: u32) -> Canvas {
    let mut out = Canvas::filled(w, h, Rgba::rgb(0, 0, 0));
    for y in 0..h {
        let fy = ((y as f32 + 0.5) / SCALE as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
        let (y0, y1) = (fy.floor() as u32, (fy.ceil() as u32).min(sh - 1));
        let ty = fy - y0 as f32;
        let top = &small[(y0 * sw) as usize..((y0 + 1) * sw) as usize];
        let bottom = &small[(y1 * sw) as usize..((y1 + 1) * sw) as usize];
        out.paint_row(y, |x| {
            let fx = ((x as f32 + 0.5) / SCALE as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
            let (x0, x1) = (fx.floor() as usize, (fx.ceil() as usize).min(sw as usize - 1));
            let tx = fx - x0 as f32;
            let mut rgb = [0u8; 3];
            for c in 0..3 {
                let hi = top[x0][c] as f32 * (1.0 - tx) + top[x1][c] as f32 * tx;
                let lo = bottom[x0][c] as f32 * (1.0 - tx) + bottom[x1][c] as f32 * tx;
                rgb[c] = (hi * (1.0 - ty) + lo * ty).round().clamp(0.0, 255.0) as u8;
            }
            Rgba::rgb(rgb[0], rgb[1], rgb[2])
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(c: &Canvas, x: u32, y: u32) -> Rgb {
        let at = ((y * c.w + x) * 4) as usize;
        [c.data[at], c.data[at + 1], c.data[at + 2]]
    }

    #[test]
    fn the_kernel_is_normalised_and_symmetric() {
        let (k, radius) = kernel(2.0);
        assert_eq!(radius, 6);
        assert!((k.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        assert!((k[0] - k[k.len() - 1]).abs() < 1e-6);
        assert!(k[radius as usize] > k[0]);
    }

    #[test]
    fn a_flat_page_stays_flat() {
        let page = Canvas::filled(320, 640, Rgba::rgb(70, 90, 130));
        let out = gaussian(&page);
        assert!(out.whole());
        assert_eq!((out.w, out.h), (320, 640));
        assert_eq!(pixel(&out, 160, 320), [70, 90, 130]);
        assert_eq!(pixel(&out, 0, 0), [70, 90, 130]);
    }

    #[test]
    fn a_bright_spot_spreads_into_its_neighbours() {
        let mut page = Canvas::filled(320, 640, Rgba::rgb(0, 0, 0));
        page.fill_circle(160.0, 320.0, 24.0, Rgba::rgb(255, 255, 255));
        let out = gaussian(&page);
        let centre = pixel(&out, 160, 320)[0] as i32;
        let away = pixel(&out, 160 + 60, 320)[0] as i32;
        let far = pixel(&out, 160 + 200, 320)[0] as i32;
        assert!(centre > 0 && centre < 255, "the spot is softened, not left sharp: {centre}");
        assert!(away > far, "light reached a neighbour: {away} vs {far}");
        assert!(far < 8, "light did not wrap around the edge: {far}");
    }

    #[test]
    fn a_hard_edge_becomes_a_ramp() {
        let mut page = Canvas::filled(320, 640, Rgba::rgb(0, 0, 0));
        page.fill_rect(160.0, 0.0, 160.0, 640.0, Rgba::rgb(255, 255, 255));
        let out = gaussian(&page);
        let samples: Vec<u8> = (120..=200).step_by(10).map(|x| pixel(&out, x, 320)[0]).collect();
        assert!(samples.windows(2).all(|w| w[0] <= w[1]), "the edge rises: {samples:?}");
        assert!(samples[0] < 80 && *samples.last().unwrap() > 200, "both sides survive: {samples:?}");
    }
}
