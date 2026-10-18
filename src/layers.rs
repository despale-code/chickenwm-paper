//! Wallpaper layers: `paper { layer = … }` lines, composited bottom to top.
//!
//! Each layer becomes a screen-sized RGBA buffer once (fit, then chroma key), so static stacks
//! cost nothing after loading. Animated (shader) layers are redrawn each frame; everything below
//! the first animated layer is cached.

use anyhow::{Context, Result};
use chickenwm_core::config::{PaperLayer, PaperLayerKind};
use image::{imageops, RgbaImage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blend {
    Normal,
    Multiply,
    Screen,
    Add,
    Overlay,
}

impl Blend {
    pub fn parse(s: &str) -> Self {
        match s {
            "multiply" => Self::Multiply,
            "screen" => Self::Screen,
            "add" | "additive" | "lighten" => Self::Add,
            "overlay" => Self::Overlay,
            _ => Self::Normal,
        }
    }

    #[inline]
    fn apply(self, b: f32, t: f32) -> f32 {
        match self {
            Self::Normal => t,
            Self::Multiply => b * t,
            Self::Screen => 1.0 - (1.0 - b) * (1.0 - t),
            Self::Add => (b + t).min(1.0),
            Self::Overlay => {
                if b < 0.5 {
                    2.0 * b * t
                } else {
                    1.0 - 2.0 * (1.0 - b) * (1.0 - t)
                }
            }
        }
    }
}

pub struct PreparedLayer {
    pub config: PaperLayer,
    /// Straight (non-premultiplied) RGBA, `width * height * 4`.
    pub rgba: Vec<u8>,
    pub blend: Blend,
    pub animated: bool,
}

/// Turns a config line into a screen-sized buffer.
pub fn prepare(
    layer: &PaperLayer,
    width: u32,
    height: u32,
    palette: &chickenwm_core::theme::Palette,
    expand: impl Fn(&str) -> std::path::PathBuf,
) -> Result<PreparedLayer> {
    let (w, h) = (width as usize, height as usize);
    let mut animated = false;
    let mut rgba = match layer.kind {
        PaperLayerKind::Image => {
            let img = image::open(expand(&layer.source))
                .with_context(|| format!("layer image {}", layer.source))?
                .to_rgba8();
            fit(&img, layer, width, height)
        }
        PaperLayerKind::Generated => chickenwm_core::wallpaper::render(palette, width, height),
        PaperLayerKind::Color => {
            let c = chickenwm_core::config::RgbaColor::parse(&layer.source)
                .with_context(|| format!("layer color {}", layer.source))?;
            [c.r, c.g, c.b, c.a].repeat(w * h)
        }
        PaperLayerKind::Shader => {
            let path = expand(&layer.source);
            if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("wgsl")) && path.exists() {
                let src = std::fs::read_to_string(&path)?;
                crate::validate_wgsl_with_wgpu(&src);
            }
            animated = true;
            let mut buf = vec![0u8; w * h * 4];
            aurora_rgba(&mut buf, width, height, 0.0);
            buf
        }
    };
    if let Some(key) = layer.key {
        chroma_key(&mut rgba, [key.r, key.g, key.b], layer.tolerance, layer.softness, layer.despill);
    }
    Ok(PreparedLayer { config: layer.clone(), rgba, blend: Blend::parse(&layer.blend), animated })
}

/// Places `img` on a transparent screen-sized canvas according to `fit`, `scale` and `x`/`y`.
fn fit(img: &RgbaImage, layer: &PaperLayer, width: u32, height: u32) -> Vec<u8> {
    let (iw, ih) = (img.width().max(1) as f32, img.height().max(1) as f32);
    let (sw, sh) = (width as f32, height as f32);
    let filter = imageops::FilterType::Triangle;
    let resized = |s: f32| imageops::resize(img, (iw * s).round().max(1.0) as u32, (ih * s).round().max(1.0) as u32, filter);
    let mut canvas = RgbaImage::new(width, height);
    let place = |canvas: &mut RgbaImage, src: &RgbaImage| {
        let x = ((sw - src.width() as f32) * layer.align_x).round() as i64;
        let y = ((sh - src.height() as f32) * layer.align_y).round() as i64;
        imageops::replace(canvas, src, x, y);
    };
    match layer.fit.as_str() {
        "stretch" | "fill" => canvas = imageops::resize(img, width, height, filter),
        "contain" | "fit" => place(&mut canvas, &resized((sw / iw).min(sh / ih))),
        "center" | "none" => place(&mut canvas, &resized(layer.scale)),
        "tile" | "repeat" => {
            let tile = resized(layer.scale);
            let (tw, th) = (tile.width() as i64, tile.height() as i64);
            let mut y = 0;
            while y < height as i64 {
                let mut x = 0;
                while x < width as i64 {
                    imageops::replace(&mut canvas, &tile, x, y);
                    x += tw;
                }
                y += th;
            }
        }
        // cover: fill the screen, crop the overflow (where depends on x / y).
        _ => {
            let big = resized((sw / iw).max(sh / ih) * layer.scale.max(1.0));
            let x = ((big.width() as f32 - sw) * layer.align_x).round().max(0.0) as u32;
            let y = ((big.height() as f32 - sh) * layer.align_y).round().max(0.0) as u32;
            canvas = imageops::crop_imm(&big, x, y, width, height).to_image();
        }
    }
    canvas.into_raw()
}

#[inline]
fn chroma(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let y = 0.299 * r + 0.587 * g + 0.114 * b;
    let cb = -0.168736 * r - 0.331264 * g + 0.5 * b;
    let cr = 0.5 * r - 0.418688 * g - 0.081312 * b;
    (y, cb, cr)
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    if e1 <= e0 {
        return if x < e0 { 0.0 } else { 1.0 };
    }
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Chroma key in YCbCr: pixels whose color (ignoring brightness) is within `tolerance` of the
/// key become transparent, with a soft edge `softness` wide. `despill` removes the key color's
/// tint from the pixels near the edge that stay.
pub fn chroma_key(rgba: &mut [u8], key: [u8; 3], tolerance: f32, softness: f32, despill: bool) {
    let f = |v: u8| v as f32 / 255.0;
    let (_, kcb, kcr) = chroma(f(key[0]), f(key[1]), f(key[2]));
    let klen = (kcb * kcb + kcr * kcr).sqrt().max(1e-5);
    let (ux, uy) = (kcb / klen, kcr / klen);
    for px in rgba.chunks_exact_mut(4) {
        if px[3] == 0 {
            continue;
        }
        let (y, cb, cr) = chroma(f(px[0]), f(px[1]), f(px[2]));
        let d = ((cb - kcb).powi(2) + (cr - kcr).powi(2)).sqrt();
        let keep = smoothstep(tolerance, tolerance + softness, d);
        px[3] = (px[3] as f32 * keep).round() as u8;
        if despill && px[3] > 0 {
            let spill = cb * ux + cr * uy;
            let w = 1.0 - smoothstep(tolerance, tolerance + softness + 0.25, d);
            if spill > 0.0 && w > 0.0 {
                let (cb, cr) = (cb - ux * spill * w, cr - uy * spill * w);
                let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                px[0] = to8(y + 1.402 * cr);
                px[1] = to8(y - 0.344136 * cb - 0.714136 * cr);
                px[2] = to8(y + 1.772 * cb);
            }
        }
    }
}

/// The animated aurora background, as straight RGBA.
pub fn aurora_rgba(buf: &mut [u8], width: u32, height: u32, time: f32) {
    let (w, h) = (width.max(1) as f32, height.max(1) as f32);
    for y in 0..height {
        let v = y as f32 / h;
        let row = (y * width) as usize * 4;
        for x in 0..width {
            let u = x as f32 / w;
            let wave = ((u * 3.5 + time * 0.4).sin() * (v * 2.5 - time * 0.3).cos()) * 0.5 + 0.5;
            let i = row + x as usize * 4;
            buf[i] = (18.0 + 25.0 * (1.0 - v) + 22.0 * wave).min(255.0) as u8;
            buf[i + 1] = (20.0 + 48.0 * wave + 12.0 * u).min(255.0) as u8;
            buf[i + 2] = (34.0 + 68.0 * wave + 30.0 * (1.0 - v)).min(255.0) as u8;
            buf[i + 3] = 255;
        }
    }
}

/// Draws `layer` over `out` (0x00RRGGBB pixels).
pub fn blend_into(out: &mut [u32], layer: &PreparedLayer) {
    let opacity = layer.config.opacity.clamp(0.0, 1.0);
    if opacity <= 0.0 {
        return;
    }
    for (dst, px) in out.iter_mut().zip(layer.rgba.chunks_exact(4)) {
        let a = px[3] as f32 / 255.0 * opacity;
        if a <= 0.0 {
            continue;
        }
        if a >= 1.0 && layer.blend == Blend::Normal {
            *dst = 0xFF00_0000 | (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32;
            continue;
        }
        let b = [(*dst >> 16) & 0xFF, (*dst >> 8) & 0xFF, *dst & 0xFF];
        let mut c = [0u32; 3];
        for i in 0..3 {
            let (bb, tt) = (b[i] as f32 / 255.0, px[i] as f32 / 255.0);
            let mixed = bb + (layer.blend.apply(bb, tt) - bb) * a;
            c[i] = (mixed.clamp(0.0, 1.0) * 255.0).round() as u32;
        }
        *dst = 0xFF00_0000 | c[0] << 16 | c[1] << 8 | c[2];
    }
}

/// Composites `layers[from..]` over `out`.
pub fn composite(out: &mut [u32], layers: &[PreparedLayer], from: usize) {
    for layer in &layers[from.min(layers.len())..] {
        blend_into(out, layer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(rgba: Vec<u8>, blend: &str, opacity: f32) -> PreparedLayer {
        PreparedLayer {
            config: PaperLayer { opacity, blend: blend.into(), ..PaperLayer::default() },
            rgba,
            blend: Blend::parse(blend),
            animated: false,
        }
    }

    #[test]
    fn key_removes_green_keeps_others() {
        let mut px = vec![0, 255, 0, 255, 220, 40, 40, 255, 200, 170, 150, 255];
        chroma_key(&mut px, [0, 255, 0], 0.25, 0.1, true);
        assert_eq!(px[3], 0, "pure green is keyed out");
        assert_eq!(px[7], 255, "red stays");
        assert_eq!(px[11], 255, "skin tone stays");
    }

    #[test]
    fn despill_reduces_green_fringe() {
        // A greenish fringe pixel just outside the tolerance.
        let mut px = vec![110, 200, 100, 255];
        chroma_key(&mut px, [0, 255, 0], 0.15, 0.1, true);
        assert!(px[1] < 200, "green tint reduced, got {:?}", px);
    }

    #[test]
    fn blend_modes() {
        let mut out = vec![0xFF80_8080u32];
        blend_into(&mut out, &layer(vec![255, 255, 255, 255], "multiply", 1.0));
        assert_eq!(out[0] & 0xFF_FFFF, 0x80_8080);
        let mut out = vec![0xFF00_0000u32];
        blend_into(&mut out, &layer(vec![255, 0, 0, 255], "normal", 0.5));
        assert_eq!((out[0] >> 16) & 0xFF, 128);
        let mut out = vec![0xFF00_0000u32];
        blend_into(&mut out, &layer(vec![255, 0, 0, 0], "normal", 1.0));
        assert_eq!(out[0] & 0xFF_FFFF, 0, "transparent pixels leave the base alone");
    }

    #[test]
    fn layer_context_filters_monitor_and_workspace() {
        let l1 = PaperLayer::parse(r"shader, aurora.wgsl, monitor = \\.\DISPLAY1, workspace = 2").unwrap();
        assert!(l1.matches_context(r"\\.\DISPLAY1", 2));
        assert!(l1.matches_context("DISPLAY1", 2));
        assert!(!l1.matches_context(r"\\.\DISPLAY2", 2));
        assert!(!l1.matches_context(r"\\.\DISPLAY1", 1));

        let global = PaperLayer::parse("generated").unwrap();
        assert!(global.matches_context(r"\\.\DISPLAY2", 5));
    }
}
