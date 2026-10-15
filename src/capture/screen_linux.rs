//! Linux screen capture.
//!
//! Displays and windows are enumerated from KWin. Frames come from an
//! xdg-desktop-portal ScreenCast stream over PipeWire. The portal, not Glide,
//! picks the source: the user chooses the monitor or window in the portal
//! dialog, and a restore token per target remembers the choice across runs.
//! Display regions are cropped in software. Mouse-click highlights have no
//! Linux mechanism and are rejected.

use crate::capture::kwin;
use crate::capture::record::Shared;
use crate::capture::types::{clock_secs, Bgra};
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use pipewire as pw;
use pw::context::ContextRc;
use pw::main_loop::MainLoopRc;
use pw::spa::buffer::meta::{MetaCursor, MetaVideoCrop};
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::param::ParamType;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Id, Rectangle, SpaTypes};
use pw::stream::{Stream, StreamFlags, StreamRc, StreamState};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::Hasher;
use std::io::Cursor;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// A monitor as reported by KWin.
#[derive(Clone, Debug)]
pub struct DisplayInfo {
    /// Stable 32-bit hash of the KWin output name.
    pub id: u32,
    /// Logical size in points.
    pub points: (i64, i64),
    /// Physical size in pixels (points x scale).
    pub pixels: (usize, usize),
}

/// A window as reported by KWin.
#[derive(Clone, Debug)]
pub struct WindowInfo {
    /// Stable 32-bit hash of the KWin window uuid.
    pub id: u32,
    pub app: String,
    pub title: String,
    /// Frame size in points.
    pub size: (f64, f64),
}

/// Stable nonzero 32-bit id from a KWin name or uuid (FNV-1a).
pub fn key32(s: &str) -> u32 {
    let h = s
        .bytes()
        .fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    h.max(1)
}

/// Displays from KWin outputs.
pub fn displays() -> Result<Vec<DisplayInfo>> {
    Ok(kwin::outputs()?
        .iter()
        .map(|o| DisplayInfo {
            id: key32(&o.name),
            points: (o.rect[2].round() as i64, o.rect[3].round() as i64),
            pixels: (
                (o.rect[2] * o.scale).round() as usize,
                (o.rect[3] * o.scale).round() as usize,
            ),
        })
        .collect())
}

/// Visible, non-minimized windows from KWin. Skips untitled, unnamed, and tiny windows.
pub fn windows() -> Result<Vec<WindowInfo>> {
    Ok(kwin::windows()?
        .into_iter()
        .filter(|w| {
            !w.minimized
                && !(w.app.is_empty() && w.title.is_empty())
                && w.rect[2] >= 40.0
                && w.rect[3] >= 40.0
        })
        .map(|w| WindowInfo {
            id: key32(&w.id),
            app: w.app,
            title: w.title,
            size: (w.rect[2], w.rect[3]),
        })
        .collect())
}

/// Resolves a window by numeric id or by case-insensitive substring of "app title".
/// The largest match wins.
pub fn find_window(sel: &str) -> Result<WindowInfo> {
    // An empty selector would match every window.
    if sel.trim().is_empty() {
        bail!("no window given (use an id from `glide devices` or part of its name)");
    }
    let ws = windows()?;
    if let Ok(id) = sel.parse::<u32>() {
        if let Some(w) = ws.into_iter().find(|w| w.id == id) {
            return Ok(w);
        }
        bail!("no on-screen window with id {id} (see `glide devices`)");
    }
    let lower = sel.to_lowercase();
    ws.into_iter()
        .filter(|w| format!("{} {}", w.app, w.title).to_lowercase().contains(&lower))
        .max_by(|a, b| (a.size.0 * a.size.1).total_cmp(&(b.size.0 * b.size.1)))
        .ok_or_else(|| anyhow!("no on-screen window matches {sel:?} (see `glide devices`)"))
}

/// What to record.
#[derive(Clone, Debug)]
pub enum Target {
    /// A display, optionally cropped to a region (points, top-left origin).
    Display(DisplayInfo, Option<[f64; 4]>),
    Window(WindowInfo),
}

impl Target {
    pub fn describe(&self) -> String {
        match self {
            Target::Display(d, None) => format!("display {}", d.id),
            Target::Display(d, Some([x, y, w, h])) => {
                format!("display {} region {w}x{h}+{x}+{y}", d.id)
            }
            Target::Window(w) => format!("window \"{} {}\" ({})", w.app, w.title, w.id),
        }
    }
}

/// Screen capture settings. Linux ignores mac-only options (system audio, mic, hiding
/// desktop icons and dock), which are not part of this struct.
#[derive(Clone, Debug)]
pub struct ScreenConfig {
    pub target: Target,
    /// Output frame size; source frames are scaled to this.
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    /// Include the cursor (sent as metadata, see `ScreenCapture::cursor_state`).
    pub cursor: bool,
    /// Mouse-click highlights. Unsupported on Linux; `start` rejects `true`.
    pub clicks: bool,
}

/// Cursor position and shape. x/y are in output frame pixels (`ScreenConfig` width/height).
#[derive(Clone, Debug)]
pub struct CursorState {
    pub x: f64,
    pub y: f64,
    pub visible: bool,
    pub bitmap: Option<CursorBitmap>,
}

/// Cursor image, BGRA, tightly packed.
#[derive(Clone, Debug)]
pub struct CursorBitmap {
    /// Hash of the pixels.
    pub id: u64,
    pub w: usize,
    pub h: usize,
    /// Hotspot in bitmap pixels.
    pub hot: (i32, i32),
    pub bgra: Vec<u8>,
}

/// A running portal ScreenCast stream. Stops on `stop` or drop.
pub struct ScreenCapture {
    session: Option<Session<Screencast>>,
    thread: Option<JoinHandle<()>>,
    halt: Arc<AtomicBool>,
    cursor: Arc<Mutex<Option<CursorState>>>,
}

impl ScreenCapture {
    /// Opens a portal ScreenCast session and starts the PipeWire stream thread.
    /// The portal dialog appears on first use (and whenever no valid restore token exists).
    pub fn start(cfg: &ScreenConfig, shared: Arc<Shared>) -> Result<ScreenCapture> {
        if cfg.width == 0 || cfg.height == 0 || cfg.fps == 0 {
            bail!("screen width, height and fps must be nonzero");
        }
        if cfg.clicks {
            bail!("mouse-click highlights are not supported on Linux: the screen stream cannot draw them");
        }
        let (key, region) = match &cfg.target {
            Target::Display(d, region) => {
                let key = format!("display-{}", d.id);
                match *region {
                    Some([x, y, w, h]) => {
                        let (dw, dh) = (d.points.0 as f64, d.points.1 as f64);
                        if !(x >= 0.0 && y >= 0.0 && w > 0.0 && h > 0.0 && x + w <= dw && y + h <= dh) {
                            bail!("region {x},{y} {w}x{h} lies outside display {}", d.id);
                        }
                        (key, Some(([x, y, w, h], (dw, dh))))
                    }
                    None => (key, None),
                }
            }
            Target::Window(w) => (format!("window-{}", w.id), None),
        };

        let token = token_path(&key);
        let (session, node, fd) = pollster::block_on(open_portal(&cfg.target, cfg.cursor, &token))?;
        let mut cap = ScreenCapture {
            session: Some(session),
            thread: None,
            halt: Arc::new(AtomicBool::new(false)),
            cursor: Arc::new(Mutex::new(None)),
        };
        let setup = StreamSetup {
            node,
            fd,
            out_w: cfg.width,
            out_h: cfg.height,
            fps: cfg.fps,
            region,
            // Pace to the requested rate; KWin delivers at refresh rate.
            min_interval: 0.9 / cfg.fps as f64,
        };
        let (halt, cursor) = (cap.halt.clone(), cap.cursor.clone());
        let thread = std::thread::Builder::new()
            .name("glide-screen".into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || {
                    if let Err(e) = run_stream(setup, Arc::clone(&shared), halt, cursor) {
                        shared.fail(format!("screen capture: {e:#}"));
                    }
                }
            })
            .context("spawning screen capture thread")?;
        cap.thread = Some(thread);
        Ok(cap)
    }

    /// Latest cursor state, or None before the first stream buffer.
    pub fn cursor_state(&self) -> Option<CursorState> {
        self.cursor.lock().expect("cursor state lock").clone()
    }

    /// A sampler reading the latest cursor state, usable from another thread.
    pub fn cursor_sampler(&self) -> impl Fn() -> Option<CursorState> + Send + 'static {
        let cursor = self.cursor.clone();
        move || cursor.lock().expect("cursor state lock").clone()
    }

    /// Stops the stream thread and closes the portal session. Idempotent.
    pub fn stop(&mut self) {
        self.halt.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            if t.join().is_err() {
                eprintln!("screen capture thread panicked");
            }
        }
        if let Some(s) = self.session.take() {
            if let Err(e) = pollster::block_on(s.close()) {
                eprintln!("closing screencast session: {e}");
            }
        }
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Restore-token file for a target, so the portal remembers the user's choice.
fn token_path(key: &str) -> PathBuf {
    util::config_dir().join(format!("screencast-{key}.token"))
}

/// Opens the portal session and returns the session, PipeWire node id, and PipeWire remote fd.
async fn open_portal(
    target: &Target,
    cursor: bool,
    token_file: &Path,
) -> Result<(Session<Screencast>, u32, OwnedFd)> {
    let proxy = Screencast::new()
        .await
        .context("connecting to the screencast portal")?;
    let session = proxy
        .create_session(Default::default())
        .await
        .context("creating screencast session")?;
    let restore = fs::read_to_string(token_file)
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty());
    let source = match target {
        Target::Display(..) => SourceType::Monitor,
        Target::Window(_) => SourceType::Window,
    };
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(if cursor {
                    CursorMode::Metadata
                } else {
                    CursorMode::Hidden
                })
                .set_sources(BitFlags::from(source))
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(restore.as_deref()),
        )
        .await
        .context("selecting screen source")?
        .response()
        .context("screen source selection was refused")?;
    let streams = proxy
        .start(&session, None, Default::default())
        .await
        .context("starting screencast")?
        .response()
        .context("screen capture was declined")?;
    let node = streams
        .streams()
        .first()
        .ok_or_else(|| anyhow!("portal returned no screen stream"))?
        .pipe_wire_node_id();
    if let Some(t) = streams.restore_token() {
        if let Some(dir) = token_file.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(token_file, t)
            .with_context(|| format!("writing {}", token_file.display()))?;
    }
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("opening PipeWire remote")?;
    Ok((session, node, fd))
}

/// Inputs for the PipeWire thread.
struct StreamSetup {
    node: u32,
    fd: OwnedFd,
    out_w: usize,
    out_h: usize,
    fps: u32,
    /// Region in points and the display's point size (for mapping to frame pixels).
    region: Option<([f64; 4], (f64, f64))>,
    min_interval: f64,
}

/// Per-stream state owned by the PipeWire thread.
struct Frames {
    shared: Arc<Shared>,
    halt: Arc<AtomicBool>,
    cursor: Arc<Mutex<Option<CursorState>>>,
    format: VideoInfoRaw,
    out_w: usize,
    out_h: usize,
    region: Option<([f64; 4], (f64, f64))>,
    min_interval: f64,
    last_push: f64,
    cursor_bitmap: Option<CursorBitmap>,
}

impl Frames {
    fn fail(&self, msg: String) {
        self.shared.fail(msg);
        self.halt.store(true, Ordering::SeqCst);
    }
}

/// Source rectangle in frame pixels.
#[derive(Clone, Copy)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// Clamps a rectangle from metadata to the frame.
fn clip(x: i32, y: i32, w: u32, h: u32, fw: usize, fh: usize) -> Rect {
    let x = (x.max(0) as usize).min(fw - 1);
    let y = (y.max(0) as usize).min(fh - 1);
    Rect {
        x,
        y,
        w: (w as usize).clamp(1, fw - x),
        h: (h as usize).clamp(1, fh - y),
    }
}

/// PipeWire thread body. Errors go to `shared.fail`.
fn run_stream(
    setup: StreamSetup,
    shared: Arc<Shared>,
    halt: Arc<AtomicBool>,
    cursor: Arc<Mutex<Option<CursorState>>>,
) -> Result<()> {
    pw::init();
    let mainloop = MainLoopRc::new(None).context("creating PipeWire main loop")?;
    let context = ContextRc::new(&mainloop, None).context("creating PipeWire context")?;
    let core = context
        .connect_fd_rc(setup.fd, None)
        .context("connecting to PipeWire remote")?;
    let stream = StreamRc::new(
        core,
        "glide-screen",
        pw::properties::properties! {
            "media.type" => "Video",
            "media.category" => "Capture",
            "media.role" => "Screen",
        },
    )
    .context("creating screen stream")?;

    let frames = Frames {
        shared: Arc::clone(&shared),
        halt: Arc::clone(&halt),
        cursor,
        format: VideoInfoRaw::new(),
        out_w: setup.out_w,
        out_h: setup.out_h,
        region: setup.region,
        min_interval: setup.min_interval,
        last_push: f64::NEG_INFINITY,
        cursor_bitmap: None,
    };
    let listener = stream
        .add_local_listener_with_user_data(frames)
        .state_changed(|_, st, _old, new| {
            if let StreamState::Error(e) = new {
                st.fail(format!("screen stream error: {e}"));
            }
        })
        .param_changed(|stream, st, id, param| {
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                return;
            }
            let mut info = VideoInfoRaw::new();
            match info.parse(param) {
                Ok(_) => st.format = info,
                Err(e) => return st.fail(format!("parsing screen format: {e:?}")),
            }
            // Without these the compositor attaches no cursor or crop metadata to
            // buffers, and in Metadata mode the cursor would then be lost entirely.
            if let Err(e) = request_meta(stream) {
                eprintln!("glide: {e:#}; recording without cursor metadata");
            }
        })
        .process(|stream, st| {
            if let Err(e) = on_frame(stream, st) {
                st.fail(format!("{e:#}"));
            }
        })
        .register()
        .context("registering screen stream listener")?;

    let format_bytes = format_pod(setup.fps)?;
    let mut params = [Pod::from_bytes(&format_bytes).ok_or_else(|| anyhow!("invalid format pod"))?];
    stream
        .connect(
            Direction::Input,
            Some(setup.node),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("connecting screen stream")?;

    // Poll the stop flag. Quit the loop when it is set.
    let weak = mainloop.downgrade();
    let (halt_t, shared_t) = (Arc::clone(&halt), Arc::clone(&shared));
    let timer = mainloop.loop_().add_timer(move |_| {
        if halt_t.load(Ordering::SeqCst) || shared_t.stopped() {
            if let Some(ml) = weak.upgrade() {
                ml.quit();
            }
        }
    });
    timer
        .update_timer(
            Some(Duration::from_millis(50)),
            Some(Duration::from_millis(50)),
        )
        .into_result()
        .context("arming screen stop timer")?;

    mainloop.run();
    drop(listener);
    Ok(())
}

/// Format pod offering BGRx/BGRA/RGBx/RGBA raw video at any size, at up to `fps`.
fn format_pod(fps: u32) -> Result<Vec<u8>> {
    let obj = pw::spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    let bytes = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|e| anyhow!("serializing format pod: {e:?}"))?
        .0
        .into_inner();
    Ok(bytes)
}

/// Size of a cursor meta whose bitmap is `w`x`h` BGRA.
const fn cursor_meta_size(w: usize, h: usize) -> i32 {
    (size_of::<pw::spa::sys::spa_meta_cursor>() + size_of::<pw::spa::sys::spa_meta_bitmap>() + w * h * 4) as i32
}

/// One SPA_PARAM_Meta pod asking for metadata of `kind` with `size` bytes.
fn meta_pod(kind: u32, size: Value) -> Result<Vec<u8>> {
    let prop = |key, value| Property { key, flags: PropertyFlags::empty(), value };
    let obj = Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            prop(pw::spa::sys::SPA_PARAM_META_type, Value::Id(Id(kind))),
            prop(pw::spa::sys::SPA_PARAM_META_size, size),
        ],
    };
    Ok(PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|e| anyhow!("serializing meta pod: {e:?}"))?
        .0
        .into_inner())
}

/// Asks the compositor for cursor and video-crop metadata on every buffer.
fn request_meta(stream: &Stream) -> Result<()> {
    let cursor = Value::Choice(ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range {
            default: cursor_meta_size(64, 64),
            min: cursor_meta_size(1, 1),
            max: cursor_meta_size(1024, 1024),
        },
    )));
    let crop = Value::Int(size_of::<pw::spa::sys::spa_meta_region>() as i32);
    let pods = [
        meta_pod(pw::spa::sys::SPA_META_Cursor, cursor)?,
        meta_pod(pw::spa::sys::SPA_META_VideoCrop, crop)?,
    ];
    let mut params: Vec<&Pod> = pods
        .iter()
        .map(|b| Pod::from_bytes(b).ok_or_else(|| anyhow!("invalid meta pod")))
        .collect::<Result<_>>()?;
    stream.update_params(&mut params).context("requesting cursor metadata")
}

/// Converts one PipeWire buffer into a BGRA frame and cursor state.
fn on_frame(stream: &Stream, st: &mut Frames) -> Result<()> {
    let Some(mut buf) = stream.dequeue_buffer() else {
        return Ok(());
    };
    let info = st.format;
    let (fw, fh) = {
        let s = info.size();
        (s.width as usize, s.height as usize)
    };
    if fw == 0 || fh == 0 {
        return Ok(());
    }
    let format = info.format();
    let now = clock_secs();

    // Crop from the compositor's video-crop metadata, if any.
    let full = Rect {
        x: 0,
        y: 0,
        w: fw,
        h: fh,
    };
    let base = match buf.find_meta::<MetaVideoCrop>() {
        Some(c) if c.meta_region().is_valid() => {
            let r = c.meta_region();
            let (p, s) = (r.position(), r.size());
            clip(p.x, p.y, s.width, s.height, fw, fh)
        }
        _ => full,
    };

    // Display region, mapped from points to base-rect pixels.
    let rect = match st.region {
        Some(([rx, ry, rw, rh], (pw_, ph))) => {
            let (sx, sy) = (base.w as f64 / pw_, base.h as f64 / ph);
            let x0 = base.x + ((rx * sx).round() as usize).min(base.w - 1);
            let y0 = base.y + ((ry * sy).round() as usize).min(base.h - 1);
            Rect {
                x: x0,
                y: y0,
                w: ((rw * sx).round() as usize).clamp(1, base.x + base.w - x0),
                h: ((rh * sy).round() as usize).clamp(1, base.y + base.h - y0),
            }
        }
        None => base,
    };

    // Cursor metadata is read on every buffer, before pacing, so the position stays current.
    let mut cursor_pos = None;
    if let Some(c) = buf.find_meta::<MetaCursor>() {
        if c.is_valid() {
            // The bitmap is attached only when the cursor image changes; keep the last one.
            if let Some(b) = cursor_bitmap(c) {
                st.cursor_bitmap = Some(b);
            }
            let p = c.position();
            cursor_pos = Some((p.x as f64, p.y as f64));
        }
    }
    {
        let mut guard = st.cursor.lock().expect("cursor state lock");
        match cursor_pos {
            Some((px, py)) => {
                *guard = Some(CursorState {
                    x: (px - rect.x as f64) * st.out_w as f64 / rect.w as f64,
                    y: (py - rect.y as f64) * st.out_h as f64 / rect.h as f64,
                    visible: true,
                    bitmap: st.cursor_bitmap.clone(),
                });
            }
            None => {
                if let Some(s) = guard.as_mut() {
                    s.visible = false;
                }
            }
        }
    }

    if now - st.last_push < st.min_interval {
        return Ok(());
    }

    let (swap, alpha) = if format == VideoFormat::BGRx {
        (false, false)
    } else if format == VideoFormat::BGRA {
        (false, true)
    } else if format == VideoFormat::RGBx {
        (true, false)
    } else if format == VideoFormat::RGBA {
        (true, true)
    } else {
        bail!("unsupported screen pixel format {:#x}", format.as_raw());
    };

    let Some(d) = buf.datas_mut().first_mut() else {
        return Ok(());
    };
    let (off, size, stride) = {
        let c = d.chunk();
        (c.offset() as usize, c.size() as usize, c.stride())
    };
    if size == 0 {
        return Ok(());
    }
    let row = if stride > 0 { stride as usize } else { fw * 4 };
    let data = d.data().context("screen buffer has no mapped memory")?;
    if row < fw * 4 || off + row * (fh - 1) + fw * 4 > data.len() {
        bail!("screen buffer too small for {fw}x{fh} frame");
    }

    // Crop and nearest-neighbour scale to the output size, converting to BGRA.
    let (ow, oh) = (st.out_w, st.out_h);
    let xmap: Vec<usize> = (0..ow).map(|ox| rect.x + ox * rect.w / ow).collect();
    let mut px = vec![0u8; ow * oh * 4];
    for oy in 0..oh {
        let srow = off + (rect.y + oy * rect.h / oh) * row;
        let drow = oy * ow * 4;
        for (ox, &sx) in xmap.iter().enumerate() {
            let s = srow + sx * 4;
            let o = drow + ox * 4;
            if swap {
                px[o] = data[s + 2];
                px[o + 1] = data[s + 1];
                px[o + 2] = data[s];
            } else {
                px[o..o + 3].copy_from_slice(&data[s..s + 3]);
            }
            px[o + 3] = if alpha { data[s + 3] } else { 255 };
        }
    }
    st.last_push = now;
    st.shared.push_screen(now, Bgra { w: ow, h: oh, px });
    Ok(())
}

/// Copies a cursor bitmap (BGRA, RGBA, ARGB or ABGR in memory; KWin sends RGBA) into
/// tightly packed BGRA. Returns None for other formats or invalid data.
fn cursor_bitmap(c: &MetaCursor) -> Option<CursorBitmap> {
    let bm = c.bitmap()?;
    let fmt = bm.format();
    // Byte offsets of B, G, R, A within one source pixel.
    let order: [usize; 4] = if fmt == VideoFormat::BGRA {
        [0, 1, 2, 3]
    } else if fmt == VideoFormat::RGBA {
        [2, 1, 0, 3]
    } else if fmt == VideoFormat::ARGB {
        [3, 2, 1, 0]
    } else if fmt == VideoFormat::ABGR {
        [1, 2, 3, 0]
    } else {
        return None;
    };
    let size = bm.size();
    let (w, h) = (size.width as usize, size.height as usize);
    let stride = bm.stride().unsigned_abs() as usize;
    let data = bm.bitmap_data()?;
    if w == 0 || h == 0 || stride < w * 4 || data.len() < stride * h {
        return None;
    }
    let mut bgra = Vec::with_capacity(w * h * 4);
    for row in data.chunks(stride).take(h) {
        for p in row[..w * 4].chunks_exact(4) {
            bgra.extend(order.map(|i| p[i]));
        }
    }
    let mut hasher = DefaultHasher::new();
    hasher.write(&bgra);
    let hs = c.hotspot();
    Some(CursorBitmap {
        id: hasher.finish(),
        w,
        h,
        hot: (hs.x, hs.y),
        bgra,
    })
}
