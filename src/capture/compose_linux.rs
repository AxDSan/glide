//! CPU compositing of the webcam over the screen (nearest sampling, BGRA).

use crate::capture::types::Bgra;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Shape {
    /// Rounded rectangle, camera aspect ratio.
    Rounded,
    /// Circle (center square crop).
    Circle,
    /// Plain rectangle.
    Rect,
}

/// What the output frame shows. Switchable live while the webcam runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scene {
    /// The screen only.
    Screen,
    /// The screen with the webcam picture-in-picture.
    #[default]
    ScreenCam,
    /// The webcam full frame.
    Cam,
}

impl Scene {
    pub const ALL: [Scene; 3] = [Scene::Screen, Scene::ScreenCam, Scene::Cam];

    pub fn label(self) -> &'static str {
        match self {
            Scene::Screen => "Screen",
            Scene::ScreenCam => "Screen + Camera",
            Scene::Cam => "Camera",
        }
    }

    pub fn uses_cam(self) -> bool {
        self != Scene::Screen
    }
}

/// Uniform scale and offset that fits (`cover` false) or fills (`cover`
/// true) a `sw`x`sh` source into a `dw`x`dh` canvas, centred.
pub fn fit(sw: f64, sh: f64, dw: f64, dh: f64, cover: bool) -> (f64, f64, f64) {
    let (a, b) = (dw / sw, dh / sh);
    let s = if cover { a.max(b) } else { a.min(b) };
    (s, (dw - sw * s) / 2.0, (dh - sh * s) / 2.0)
}

#[derive(Clone, Debug)]
pub struct PipSpec {
    /// Overlay width as a fraction of the output width.
    pub size: f64,
    pub corner: Corner,
    pub shape: Shape,
    pub mirror: bool,
}

pub struct Compositor {
    w: usize,
    h: usize,
    pip: PipSpec,
}

/// A frame with a usable buffer.
fn valid(b: &Bgra) -> bool {
    b.w > 0 && b.h > 0 && b.px.len() >= b.w * b.h * 4
}

/// Anti-aliased coverage (0..1) of the point (px, py) inside the rounded
/// rectangle `[x, y, w, h]` with corner radius `radius` (clamped to fit).
fn rounded_cover(px: f64, py: f64, [x, y, w, h]: [f64; 4], radius: f64) -> f64 {
    let r = radius.clamp(0.0, w.min(h) / 2.0);
    let qx = (px - (x + w / 2.0)).abs() - (w / 2.0 - r);
    let qy = (py - (y + h / 2.0)).abs() - (h / 2.0 - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let d = outside + qx.max(qy).min(0.0) - r;
    (0.5 - d).clamp(0.0, 1.0)
}

/// Copy the `from` region `[x, y, w, h]` of `src` (source pixels) into the
/// `to` rect of `out` (canvas pixels; parts outside the canvas are clipped).
/// Nearest sampling, mirrored left-right within `to` if asked. Each pixel is
/// blended over the canvas by `cover(px, py)` (0..1).
fn blit(out: &mut Bgra, src: &Bgra, from: [f64; 4], to: [f64; 4], mirror: bool, cover: impl Fn(f64, f64) -> f64) {
    let [fx, fy, fw, fh] = from;
    let [tx, ty, tw, th] = to;
    if tw <= 0.0 || th <= 0.0 {
        return;
    }
    let x0 = tx.floor().max(0.0) as usize;
    let y0 = ty.floor().max(0.0) as usize;
    let x1 = ((tx + tw).ceil().max(0.0) as usize).min(out.w);
    let y1 = ((ty + th).ceil().max(0.0) as usize).min(out.h);
    for py in y0..y1 {
        let cy = py as f64 + 0.5;
        let sy = (fy + (cy - ty) / th * fh).floor().clamp(0.0, src.h as f64 - 1.0) as usize;
        for px in x0..x1 {
            let cx = px as f64 + 0.5;
            let a = cover(cx, cy);
            if a <= 0.0 {
                continue;
            }
            let mut u = (cx - tx) / tw;
            if mirror {
                u = 1.0 - u;
            }
            let sx = (fx + u * fw).floor().clamp(0.0, src.w as f64 - 1.0) as usize;
            let s = &src.px[(sy * src.w + sx) * 4..][..4];
            let d = &mut out.px[(py * out.w + px) * 4..][..4];
            if a >= 1.0 {
                d[..3].copy_from_slice(&s[..3]);
            } else {
                for c in 0..3 {
                    d[c] = (s[c] as f64 * a + d[c] as f64 * (1.0 - a)).round() as u8;
                }
            }
            d[3] = 255;
        }
    }
}

impl Compositor {
    pub fn new(width: usize, height: usize, pip: PipSpec) -> Self {
        Compositor { w: width, h: height, pip }
    }

    /// Compose `scene` into `out` (resized to the canvas; black where nothing
    /// is drawn). `screen` and `cam` are the latest frames, if any.
    pub fn render(&mut self, scene: Scene, screen: Option<&Bgra>, cam: Option<&Bgra>, out: &mut Bgra) {
        let (w, h) = (self.w, self.h);
        let (wf, hf) = (w as f64, h as f64);
        out.w = w;
        out.h = h;
        out.px.clear();
        out.px.resize(w * h * 4, 0);
        for p in out.px.chunks_exact_mut(4) {
            p[3] = 255;
        }
        if scene == Scene::Cam {
            if let Some(c) = cam.filter(|c| valid(c)) {
                let (cw, ch) = (c.w as f64, c.h as f64);
                let (s, tx, ty) = fit(cw, ch, wf, hf, true);
                blit(out, c, [0.0, 0.0, cw, ch], [tx, ty, cw * s, ch * s], self.pip.mirror, |_, _| 1.0);
            }
            return;
        }
        if let Some(sc) = screen.filter(|s| valid(s)) {
            let (sw, sh) = (sc.w as f64, sc.h as f64);
            let (s, tx, ty) = fit(sw, sh, wf, hf, false);
            blit(out, sc, [0.0, 0.0, sw, sh], [tx, ty, sw * s, sh * s], false, |_, _| 1.0);
        }
        if scene != Scene::ScreenCam {
            return;
        }
        let Some(c) = cam.filter(|c| valid(c)) else {
            return;
        };
        let pip = &self.pip;
        let (cx, cy, cw, ch) = if pip.shape == Shape::Circle {
            let side = c.w.min(c.h) as f64;
            ((c.w as f64 - side) / 2.0, (c.h as f64 - side) / 2.0, side, side)
        } else {
            (0.0, 0.0, c.w as f64, c.h as f64)
        };
        let tw = (wf * pip.size).round();
        let s = tw / cw;
        let th = (ch * s).round();
        let margin = (wf * 0.02).round();
        let x = match pip.corner {
            Corner::BottomRight | Corner::TopRight => wf - margin - tw,
            _ => margin,
        };
        // Top-left origin: bottom corners sit `margin` above the bottom edge.
        let y = match pip.corner {
            Corner::BottomRight | Corner::BottomLeft => hf - margin - th,
            _ => margin,
        };
        let target = [x, y, tw, th];
        let radius = match pip.shape {
            Shape::Circle => tw / 2.0,
            Shape::Rounded => tw * 0.06,
            Shape::Rect => 0.0,
        };
        blit(out, c, [cx, cy, cw, ch], target, pip.mirror, |px, py| rounded_cover(px, py, target, radius));
    }
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn fit_letterboxes_and_cover_crops() {
        // 4:3 camera into a 16:9 canvas.
        let (s, tx, ty) = fit(1280.0, 960.0, 1920.0, 1080.0, false);
        assert_eq!((s, tx, ty), (1.125, 240.0, 0.0));
        let (s, tx, ty) = fit(1280.0, 960.0, 1920.0, 1080.0, true);
        assert_eq!((s, tx, ty), (1.5, 0.0, -180.0));
    }
}
