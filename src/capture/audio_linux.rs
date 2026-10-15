//! PipeWire audio: device enumeration, system-audio and microphone capture.
//! Each capture owns one thread with its own main loop; PipeWire objects are
//! `Rc`-based and never leave that thread.

use crate::capture::record::Shared;
use crate::capture::types::{AudioKind, clock_secs};
use anyhow::{Result, anyhow, bail};
use pipewire as pw;
use pw::context::ContextRc;
use pw::core::{Core, CoreRc, PW_ID_CORE};
use pw::keys;
use pw::main_loop::MainLoopRc;
use pw::metadata::Metadata;
use pw::properties::PropertiesBox;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
use pw::spa::pod::{Object, Pod, Value, serialize::PodSerializer};
use pw::spa::sys as spa_sys;
use pw::spa::utils::Direction;
use pw::stream::{StreamBox, StreamFlags, StreamListener, StreamState};
use pw::types::ObjectType;
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::io::Cursor;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use super::camera::Device;

/// Sample rate of every audio stream the recorder sees.
pub(super) const RATE: u32 = 48_000;
/// How often stream threads check their stop flags.
pub(super) const TICK: Duration = Duration::from_millis(100);

/// Initialises the PipeWire library once per process.
pub(super) fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(pw::init);
}

/// A main loop, context and core connected to the local PipeWire server.
pub(super) fn connect() -> Result<(MainLoopRc, ContextRc, CoreRc)> {
    init();
    let mainloop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    Ok((mainloop, context, core))
}

struct Snapshot {
    sources: Vec<Device>,
    default: Option<String>,
}

/// Reads the audio source nodes and the default source name over one short-lived
/// connection. Two core round trips guarantee the registry has delivered every global.
fn snapshot() -> Result<Snapshot> {
    let (mainloop, _context, core) = connect()?;
    let registry = core.get_registry_rc()?;

    let sources: Rc<RefCell<Vec<Device>>> = Rc::default();
    let default: Rc<RefCell<Option<String>>> = Rc::default();
    // Metadata proxies and their listeners must outlive the round trips below.
    let keep: Rc<RefCell<Vec<Box<dyn Any>>>> = Rc::default();

    let reg = registry.clone();
    let (src, def, kept) = (sources.clone(), default.clone(), keep.clone());
    let _registry_listener = registry
        .add_listener_local()
        .global(move |g| {
            let Some(props) = g.props else { return };
            if matches!(g.type_, ObjectType::Node) {
                let class = props.get(*keys::MEDIA_CLASS).unwrap_or("");
                if !class.starts_with("Audio/Source") {
                    return;
                }
                let Some(id) = props.get(*keys::NODE_NAME) else { return };
                let name = props
                    .get(*keys::NODE_DESCRIPTION)
                    .or(props.get(*keys::NODE_NICK))
                    .unwrap_or(id);
                src.borrow_mut().push(Device {
                    id: id.to_owned(),
                    name: name.to_owned(),
                });
            } else if matches!(g.type_, ObjectType::Metadata)
                && props.get("metadata.name") == Some("default")
            {
                match reg.bind::<Metadata, _>(g) {
                    Ok(meta) => {
                        let def = def.clone();
                        let listener = meta
                            .add_listener_local()
                            .property(move |_subject, key, _ty, value| {
                                if key == Some("default.audio.source") {
                                    *def.borrow_mut() = value.and_then(default_name);
                                }
                                0
                            })
                            .register();
                        kept.borrow_mut().push(Box::new((meta, listener)));
                    }
                    Err(e) => eprintln!("glide: default audio metadata: {e}"),
                }
            }
        })
        .register();

    let finished = Rc::new(Cell::new(false));
    let rounds = Rc::new(Cell::new(0u32));
    let (ml, c2, fin, rd) = (mainloop.clone(), core.clone(), finished.clone(), rounds.clone());
    let _core_listener = core
        .add_listener_local()
        .done(move |id, _seq| {
            if id != PW_ID_CORE {
                return;
            }
            rd.set(rd.get() + 1);
            if rd.get() == 1 {
                if c2.sync(0).is_err() {
                    ml.quit();
                }
            } else {
                fin.set(true);
                ml.quit();
            }
        })
        .register();
    core.sync(0)?;

    let ml = mainloop.clone();
    let deadline = mainloop.loop_().add_timer(move |_| ml.quit());
    deadline
        .update_timer(Some(Duration::from_secs(2)), None)
        .into_result()?;
    mainloop.run();

    if !finished.get() {
        bail!("PipeWire did not answer within 2 s");
    }
    Ok(Snapshot {
        sources: sources.take(),
        default: default.take(),
    })
}

/// `snapshot`, logging failure and yielding an empty list (the mac API is infallible).
fn snapshot_or_empty() -> Snapshot {
    snapshot().unwrap_or_else(|e| {
        eprintln!("glide: audio devices: {e:#}");
        Snapshot {
            sources: Vec::new(),
            default: None,
        }
    })
}

fn default_name(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("name")?.as_str().map(str::to_owned)
}

/// Audio input sources. `id` is the PipeWire node name, `name` its description.
pub fn microphones() -> Vec<Device> {
    snapshot_or_empty().sources
}

/// Node name of the default audio source, if it is listed in `microphones()`.
pub fn default_microphone() -> Option<String> {
    let s = snapshot_or_empty();
    let d = s.default?;
    s.sources.iter().any(|m| m.id == d).then_some(d)
}

/// Resolve a user-supplied mic selector (unique ID, or case-insensitive name
/// substring) to a unique ID.
pub fn resolve_mic(sel: &str) -> Result<String> {
    let mics = snapshot()?.sources;
    if let Some(m) = mics.iter().find(|m| m.id == sel) {
        return Ok(m.id.clone());
    }
    let lower = sel.to_lowercase();
    mics.iter()
        .find(|m| m.name.to_lowercase().contains(&lower))
        .map(|m| m.id.clone())
        .ok_or_else(|| anyhow!("no microphone matches {sel:?} (see `glide devices`)"))
}

/// Stream callback state: the caller's data plus the negotiated channel count.
pub(super) struct Tap<D> {
    data: D,
    channels: u32,
    on_frames: Box<dyn FnMut(&mut D, f64, &[[f32; 2]])>,
    on_error: Box<dyn FnMut(&mut D, String)>,
}

/// Serialises the stereo F32 format request sent to the server.
fn format_pod() -> Result<Vec<u8>> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(2);
    let mut position = [0; MAX_CHANNELS];
    position[0] = spa_sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa_sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = Value::Object(Object {
        type_: spa_sys::SPA_TYPE_OBJECT_Format,
        id: spa_sys::SPA_PARAM_EnumFormat,
        properties: info.into(),
    });
    let (cursor, _) = PodSerializer::serialize(Cursor::new(Vec::new()), &obj)
        .map_err(|e| anyhow!("audio format pod: {e:?}"))?;
    Ok(cursor.into_inner())
}

/// Opens a stereo F32 input stream. `on_frames` receives each block with the
/// timestamp of its first frame; `on_error` receives stream failures.
/// `capture_sink` taps the default output monitor; otherwise `target` names the
/// source node (None = default source).
pub(super) fn input_stream<'c, D: 'static>(
    core: &'c Core,
    name: &str,
    target: Option<&str>,
    capture_sink: bool,
    data: D,
    on_frames: impl FnMut(&mut D, f64, &[[f32; 2]]) + 'static,
    on_error: impl FnMut(&mut D, String) + 'static,
) -> Result<(StreamBox<'c>, StreamListener<Tap<D>>)> {
    let mut props = PropertiesBox::new();
    props.insert(*keys::MEDIA_TYPE, "Audio");
    props.insert(*keys::MEDIA_CATEGORY, "Capture");
    props.insert(*keys::MEDIA_ROLE, "Production");
    props.insert(*keys::AUDIO_CHANNELS, "2");
    props.insert("audio.rate", RATE.to_string());
    if capture_sink {
        props.insert(*keys::STREAM_CAPTURE_SINK, "true");
    }
    if let Some(t) = target {
        props.insert("target.object", t);
    }
    let stream = StreamBox::new(core, name, props)?;

    let tap = Tap {
        data,
        channels: 2,
        on_frames: Box::new(on_frames),
        on_error: Box::new(on_error),
    };
    let listener = stream
        .add_local_listener_with_user_data(tap)
        .state_changed(|_, tap, _old, new| {
            if let StreamState::Error(msg) = new {
                (tap.on_error)(&mut tap.data, msg);
            }
        })
        .param_changed(|_, tap, id, param| {
            if id != spa_sys::SPA_PARAM_Format {
                return;
            }
            let Some(param) = param else { return };
            let mut info = AudioInfoRaw::new();
            if info.parse(param).is_err() || info.format().0 != AudioFormat::F32LE.0 {
                (tap.on_error)(
                    &mut tap.data,
                    "audio stream negotiated an unsupported sample format".into(),
                );
                return;
            }
            tap.channels = info.channels();
        })
        .process(|stream, tap| {
            let Some(mut buf) = stream.dequeue_buffer() else { return };
            let Some(data) = buf.datas_mut().first_mut() else { return };
            let (off, size) = {
                let c = data.chunk();
                (c.offset() as usize, c.size() as usize)
            };
            let Some(bytes) = data.data() else { return };
            let Some(bytes) = bytes.get(off..off.saturating_add(size)) else { return };
            let ch = tap.channels.max(1) as usize;
            let frames: Vec<[f32; 2]> = bytes
                .chunks_exact(4 * ch)
                .map(|f| {
                    let s = |i: usize| {
                        f32::from_le_bytes([f[4 * i], f[4 * i + 1], f[4 * i + 2], f[4 * i + 3]])
                    };
                    let l = s(0);
                    [l, if ch > 1 { s(1) } else { l }]
                })
                .collect();
            if frames.is_empty() {
                return;
            }
            let pts = clock_secs() - frames.len() as f64 / RATE as f64;
            (tap.on_frames)(&mut tap.data, pts, frames.as_slice());
        })
        .register()?;

    let pod = format_pod()?;
    let mut params = [Pod::from_bytes(&pod).ok_or_else(|| anyhow!("invalid audio format pod"))?];
    stream.connect(
        Direction::Input,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    Ok((stream, listener))
}

fn push_to(shared: Arc<Shared>) -> impl FnMut(&mut AudioKind, f64, &[[f32; 2]]) + 'static {
    move |kind, pts, frames| shared.push_audio(*kind, pts, frames.to_vec())
}

fn fail_to(shared: Arc<Shared>) -> impl FnMut(&mut AudioKind, String) + 'static {
    move |_, msg| shared.fail(msg)
}

/// Live system-audio and/or microphone capture. Dropping it stops the PipeWire thread.
pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    /// `system`: capture the default output. `mic`: None = no microphone,
    /// Some(None) = default source, Some(Some(node)) = that source node.
    pub fn start(
        system: bool,
        mic: Option<Option<String>>,
        shared: Arc<Shared>,
    ) -> Result<AudioCapture> {
        if !system && mic.is_none() {
            bail!("nothing to capture: enable system audio or a microphone");
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || run(system, mic, shared, stop, ready_tx))
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(AudioCapture {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                bail!("audio capture: {e}")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::Relaxed);
                bail!("audio capture timed out")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("audio thread exited during setup"),
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            if t.join().is_err() {
                eprintln!("glide: audio thread panicked");
            }
        }
    }
}

fn run(
    system: bool,
    mic: Option<Option<String>>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    if let Err(e) = capture(system, mic, &shared, stop, &ready) {
        let _ = ready.send(Err(format!("{e:#}")));
    }
}

fn capture(
    system: bool,
    mic: Option<Option<String>>,
    shared: &Arc<Shared>,
    stop: Arc<AtomicBool>,
    ready: &mpsc::Sender<Result<(), String>>,
) -> Result<()> {
    let (mainloop, _context, core) = connect()?;
    let _system_stream = if system {
        Some(input_stream(
            &core,
            "glide system audio",
            None,
            true,
            AudioKind::System,
            push_to(shared.clone()),
            fail_to(shared.clone()),
        )?)
    } else {
        None
    };
    let _mic_stream = match mic {
        None => None,
        Some(target) => Some(input_stream(
            &core,
            "glide microphone",
            target.as_deref(),
            false,
            AudioKind::Mic,
            push_to(shared.clone()),
            fail_to(shared.clone()),
        )?),
    };

    let ml = mainloop.clone();
    let (shared_t, stop_t) = (shared.clone(), stop);
    let tick = mainloop.loop_().add_timer(move |_| {
        if stop_t.load(Ordering::Relaxed) || shared_t.stopped() {
            ml.quit();
        }
    });
    tick.update_timer(Some(TICK), Some(TICK)).into_result()?;
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}
