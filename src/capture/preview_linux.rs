//! Live preview of a capture source for the recording-setup canvas: a
//! separate screen-cast or camera stream that only keeps the newest frame.
//! Screen previews can also report the level of the system audio they'd record.
//!
//! Frames are converted to RGBA at the canvas size (`set_target`) on the
//! capture thread, nearest-neighbour; the UI copies them with `take_frame`.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use super::{
    audio::AudioCapture,
    camera::Camera,
    record::Shared,
    screen::{self, ScreenCapture, ScreenConfig, Target},
    types::Bgra,
};

/// What to preview (display ids as KWin reports them; 0 = main).
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Display { id: u32 },
    /// A region of a display in points (x, y, w, h, top-left origin).
    Area { display: u32, rect: [f64; 4] },
    Window { id: u32 },
    /// A camera (None = the default one).
    Camera { id: Option<String> },
    /// A USB-connected iPhone or iPad's screen (macOS only).
    Device { id: String },
}

/// One preview frame, RGBA.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The pixel size to show a `fw`x`fh` frame at in a canvas of `area` pixels:
/// its aspect ratio fitted into the area, never larger than the frame.
pub fn target_size(fw: usize, fh: usize, area: (u32, u32)) -> (usize, usize) {
    if area.0 < 2 || area.1 < 2 {
        return (fw, fh);
    }
    let s = (area.0 as f64 / fw as f64).min(area.1 as f64 / fh as f64).min(1.0);
    (((fw as f64 * s).round() as usize).max(2) & !1, ((fh as f64 * s).round() as usize).max(2) & !1)
}

/// A running preview; stops when dropped.
pub struct LivePreview {
    latest: Arc<Mutex<Option<Frame>>>,
    level: Arc<AtomicU32>,
    target: Arc<AtomicU64>,
    _screen: Option<ScreenCapture>,
    _camera: Option<Camera>,
    _audio: Option<AudioCapture>,
}

/// Pixels per point of a display (at least 1).
fn backing(d: &screen::DisplayInfo) -> f64 {
    (d.pixels.0 as f64 / d.points.0.max(1) as f64).max(1.0)
}

/// An even pixel count of at least 2.
fn even(v: f64) -> usize {
    ((v.round() as usize) / 2 * 2).max(2)
}

/// BGRA `f` nearest-neighbour scaled to `w`x`h` RGBA (alpha 255).
fn to_rgba(f: &Bgra, w: usize, h: usize) -> Frame {
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row = &f.px[(y * f.h / h) * f.w * 4..][..f.w * 4];
        for x in 0..w {
            let p = &row[(x * f.w / w) * 4..][..4];
            rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
        }
    }
    Frame { width: w as u32, height: h as u32, rgba }
}

impl LivePreview {
    /// Start previewing `source` at `fps` frames per second, calling
    /// `on_frame` (from a capture thread) after each new frame. Screens are
    /// captured at their native pixel size and scaled to `set_target`;
    /// cameras to at most `max_px` on the long edge. With `audio`, a screen
    /// preview also measures the system audio it would record (`take_level`).
    pub fn start(source: &Source, max_px: u32, fps: u32, audio: bool, on_frame: impl Fn() + Send + Sync + 'static) -> Result<LivePreview> {
        let (target, (width, height)) = match source {
            Source::Camera { id } => return Self::start_camera(id.as_deref(), max_px, on_frame),
            Source::Device { .. } => bail!("iPhone and iPad previews need macOS"),
            Source::Display { id } | Source::Area { display: id, .. } => {
                let d = screen::displays()?
                    .into_iter()
                    .find(|d| d.id == *id || *id == 0)
                    .context("display not found")?;
                let px = d.pixels;
                match source {
                    Source::Area { rect, .. } => {
                        if rect[2] < 2.0 || rect[3] < 2.0 {
                            bail!("nothing to preview");
                        }
                        let scale = backing(&d);
                        (Target::Display(d, Some(*rect)), (even(rect[2] * scale), even(rect[3] * scale)))
                    }
                    _ => (Target::Display(d, None), (even(px.0 as f64), even(px.1 as f64))),
                }
            }
            Source::Window { id } => {
                let scale = screen::displays().ok().as_ref().and_then(|ds| ds.first().map(backing)).unwrap_or(1.0);
                let w = screen::windows()?
                    .into_iter()
                    .find(|w| w.id == *id)
                    .context("that window is no longer on screen")?;
                if w.size.0 < 2.0 || w.size.1 < 2.0 {
                    bail!("nothing to preview");
                }
                let size = (even(w.size.0 * scale), even(w.size.1 * scale));
                (Target::Window(w), size)
            }
        };
        // Native pixels, as the screen delivers them; scaled down on delivery.
        let (shared, mut preview) = Self::sink((u32::MAX, u32::MAX), on_frame);
        let cfg = ScreenConfig { target, width, height, fps: fps.max(1), cursor: false, clicks: false };
        preview._screen = Some(ScreenCapture::start(&cfg, shared.clone())?);
        if audio {
            preview._audio = Some(AudioCapture::start(true, None, shared)?);
        }
        Ok(preview)
    }

    /// Preview a camera (frames scaled to at most `max_px` on the long edge).
    fn start_camera(sel: Option<&str>, max_px: u32, on_frame: impl Fn() + Send + Sync + 'static) -> Result<LivePreview> {
        let (shared, mut preview) = Self::sink((max_px, max_px), on_frame);
        preview._camera = Some(Camera::start(sel, shared)?);
        Ok(preview)
    }

    /// A preview with no capture attached yet, and the shared sink feeding it.
    /// `fallback` is the area frames are fitted into until `set_target` is called.
    fn sink(fallback: (u32, u32), on_frame: impl Fn() + Send + Sync + 'static) -> (Arc<Shared>, LivePreview) {
        let latest = Arc::new(Mutex::new(None));
        let level = Arc::new(AtomicU32::new(0));
        let target = Arc::new(AtomicU64::new(0));
        let shared = {
            let (latest, target) = (latest.clone(), target.clone());
            let level = level.clone();
            Shared::new_preview(
                move |f: &Bgra| {
                    if f.w < 2 || f.h < 2 || f.px.len() < f.w * f.h * 4 {
                        return;
                    }
                    let t = target.load(Ordering::Relaxed);
                    let area = if t == 0 { fallback } else { ((t >> 32) as u32, t as u32) };
                    let (w, h) = target_size(f.w, f.h, area);
                    *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(to_rgba(f, w, h));
                    on_frame();
                },
                move |frames: &[[f32; 2]]| {
                    // Non-negative floats order like their bits, so fetch_max is a peak.
                    let peak = frames.iter().flatten().fold(0f32, |m, v| m.max(v.abs()));
                    level.fetch_max(peak.to_bits(), Ordering::Relaxed);
                },
            )
        };
        (shared, LivePreview { latest, level, target, _screen: None, _camera: None, _audio: None })
    }

    /// Peak audio level (0..1) since the last call; 0 without audio.
    pub fn take_level(&self) -> f32 {
        f32::from_bits(self.level.swap(0, Ordering::Relaxed))
    }

    /// Show frames at `w`x`h` pixels at most (the canvas; aspect ratio kept,
    /// never enlarged). 0 keeps screens at their native size.
    pub fn set_target(&self, w: u32, h: u32) {
        self.target.store((w as u64) << 32 | h as u64, Ordering::Relaxed);
    }

    /// The newest frame since the last call, as RGBA.
    pub fn take_frame(&self) -> Option<Frame> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}
