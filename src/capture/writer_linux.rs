//! Linux writer: one ffmpeg process encodes the composed video (stdin) and each
//! audio track (its own pipe at fd 3+i). The timeline matches the macOS
//! `writer::Sink`: a fixed frame grid, paused spans cut out, and every audio
//! track placed on that grid.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use super::compose::{Compositor, Scene};
pub use super::types::{AudioLayout, Codec, Stats, VideoSpec};
use super::types::{clock_secs, AudioKind, Bgra, Pauses, RATE};
use crate::media::ffmpeg::{encoder_args, filter_threads, video_encoder, video_filter, VideoKind};

/// Frames kept per camera or screen queue.
const QUEUE: usize = 4;
/// Seconds between preview callbacks (30 fps).
const PREVIEW_GAP: f64 = 1.0 / 30.0;
/// Samples the mix may wait for a lagging lane before writing without it (0.5 s).
const MAX_WAIT: i64 = 24000;
/// Sample rate as an integer, for sample-index maths.
const RATE_I: i64 = 48000;
/// Lower bound of the preview lag (seconds).
pub const LAG_SECS: f64 = 0.05;

/// Receives every emitted (composited) video frame, throttled to ~30 fps.
pub type PreviewFn = Arc<dyn Fn(&Bgra) + Send + Sync>;

#[derive(Clone, Copy, PartialEq)]
enum Track {
    Mix,
    Stem(AudioKind),
}

fn lane_of(kind: AudioKind) -> usize {
    (kind == AudioKind::Mic) as usize
}

/// One audio file track: its kind, feeding thread, and the next sample it expects.
struct AudioTrack {
    track: Track,
    tx: Sender<Vec<u8>>,
    pos: i64,
}

impl AudioTrack {
    fn uses(&self, lane: usize) -> bool {
        match self.track {
            Track::Mix => true,
            Track::Stem(k) => lane_of(k) == lane,
        }
    }

    fn mono(&self) -> bool {
        matches!(self.track, Track::Stem(AudioKind::Mic))
    }
}

/// One audio source on the output sample grid. Holds samples `[next - len, next)`.
#[derive(Default)]
struct Lane {
    next: i64,
    buf: VecDeque<[f32; 2]>,
}

impl Lane {
    fn push(&mut self, mut idx: i64, mut frames: &[[f32; 2]]) {
        if idx < self.next {
            let skip = ((self.next - idx) as usize).min(frames.len());
            frames = &frames[skip..];
            idx += skip as i64;
        }
        self.pad_to(idx);
        self.buf.extend(frames.iter().copied());
        self.next += frames.len() as i64;
    }

    /// Appends silence up to sample `j`.
    fn pad_to(&mut self, j: i64) {
        if j > self.next {
            let len = self.buf.len() as i64 + (j - self.next);
            self.buf.resize(len as usize, [0.0; 2]);
            self.next = j;
        }
    }

    fn get(&self, j: i64) -> [f32; 2] {
        let base = self.next - self.buf.len() as i64;
        if j >= base && j < self.next {
            self.buf[(j - base) as usize]
        } else {
            [0.0; 2]
        }
    }

    /// Drops samples before `keep`.
    fn trim(&mut self, keep: i64) {
        let base = self.next - self.buf.len() as i64;
        let n = (keep - base).clamp(0, self.buf.len() as i64) as usize;
        self.buf.drain(..n);
    }
}

/// The ffmpeg process and its writer threads.
struct Out {
    video: SyncSender<Arc<Bgra>>,
    audio: Vec<AudioTrack>,
    child: Child,
    writers: Vec<JoinHandle<Result<()>>>,
    log: JoinHandle<String>,
    path: PathBuf,
    tracks: Vec<Track>,
}

/// Raw f32le file holding audio track `i`, next to the movie.
fn audio_path(path: &Path, i: usize) -> PathBuf {
    path.with_extension(format!("audio{i}.f32"))
}

/// Video-only intermediate; `mux` adds the audio tracks to make the final movie.
fn video_path(path: &Path) -> PathBuf {
    path.with_extension("video.mov")
}

impl Out {
    fn open(path: &Path, v: &VideoSpec, tracks: &[Track], recycle: Sender<Bgra>) -> Result<Self> {
        let kind = match v.codec {
            Codec::H264 => VideoKind::H264,
            Codec::Hevc => VideoKind::Hevc,
            Codec::Prores => VideoKind::Prores422,
        };
        let size = format!("{}x{}", v.width, v.height);
        let fps = v.fps.to_string();
        let mut args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-nostats".into(),
        ];
        args.extend(filter_threads());
        args.extend([
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            "bgra".into(),
            "-video_size".into(),
            size,
            "-framerate".into(),
            fps,
            "-i".into(),
            "pipe:0".into(),
        ]);
        args.extend(["-map", "0:v:0"].map(String::from));
        // H.264/HEVC: no BT.709 filter. NVENC takes the BGRA as is and converts
        // on the GPU; a software encoder gets ffmpeg's default conversion. That is
        // about half the CPU, and it round-trips more faithfully through Glide's
        // reader, which decodes with the same default matrix.
        match kind {
            VideoKind::H264 | VideoKind::Hevc if video_encoder(kind).ends_with("_nvenc") => {}
            VideoKind::H264 | VideoKind::Hevc => args.extend(["-pix_fmt", "yuv420p"].map(String::from)),
            _ => args.extend(["-vf", video_filter(kind)].map(String::from)),
        }
        args.extend(encoder_args(kind, v.quality, v.bitrate, v.fps, true));
        args.extend(
            ["-movflags", "+frag_keyframe+empty_moov+default_base_moof", "-fps_mode", "cfr"]
                .map(String::from),
        );

        let mut child = Command::new("ffmpeg")
            .args(&args)
            .arg(video_path(path))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start ffmpeg")?;
        let stdin: ChildStdin = child.stdin.take().context("ffmpeg stdin missing")?;
        let mut stderr = child.stderr.take().context("ffmpeg stderr missing")?;

        let (video, vrx) = mpsc::sync_channel::<Arc<Bgra>>(4);
        let mut writers = vec![std::thread::spawn(move || -> Result<()> {
            let mut stdin = stdin;
            for frame in vrx {
                stdin.write_all(&frame.px)?;
                if let Ok(buf) = Arc::try_unwrap(frame) {
                    let _ = recycle.send(buf);
                }
            }
            Ok(())
        })];

        // Audio goes to raw files, so ffmpeg never waits on audio while encoding video.
        let mut audio = Vec::with_capacity(tracks.len());
        for (i, t) in tracks.iter().enumerate() {
            let mut file = File::create(audio_path(path, i)).context("failed to create audio file")?;
            let (tx, rx) = mpsc::channel::<Vec<u8>>();
            writers.push(std::thread::spawn(move || -> Result<()> {
                for buf in rx {
                    file.write_all(&buf)?;
                }
                Ok(())
            }));
            audio.push(AudioTrack { track: *t, tx, pos: 0 });
        }

        let log = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        Ok(Out { video, audio, child, writers, log, path: path.to_path_buf(), tracks: tracks.to_vec() })
    }

    /// Ends the streams, finalizes the video, then muxes the audio in, cut to `secs`.
    fn close(self, secs: f64) -> Result<()> {
        let Out { video, audio, mut child, writers, log, path, tracks } = self;
        drop(video);
        drop(audio);
        let mut err: Option<anyhow::Error> = None;
        for w in writers {
            match w.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => err = err.or(Some(e)),
                Err(_) => err = err.or(Some(anyhow!("writer thread panicked"))),
            }
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                bail!("timed out finalizing movie");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let log = log.join().unwrap_or_default();
        if let Some(e) = err {
            bail!("ffmpeg failed ({e:#}): {}", log.trim());
        }
        if !status.success() {
            bail!("ffmpeg failed ({status}): {}", log.trim());
        }
        mux(&path, &tracks, secs)
    }
}

/// Writes the final movie: the video intermediate plus each raw audio file as an AAC track,
/// cut to `secs`. Intermediates are removed on success.
fn mux(path: &Path, tracks: &[Track], secs: f64) -> Result<()> {
    let video = video_path(path);
    if tracks.is_empty() {
        std::fs::rename(&video, path).context("failed to move movie into place")?;
        return Ok(());
    }
    let tmp = path.with_extension("mux.mov");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostats", "-y", "-i"]).arg(&video);
    for (i, t) in tracks.iter().enumerate() {
        let ch = if matches!(t, Track::Stem(AudioKind::Mic)) { "1" } else { "2" };
        cmd.args(["-f", "f32le", "-ar", "48000", "-ac", ch, "-i"]);
        cmd.arg(audio_path(path, i));
    }
    cmd.args(["-map", "0:v:0"]);
    for (i, t) in tracks.iter().enumerate() {
        let bitrate = if matches!(t, Track::Stem(AudioKind::Mic)) { "128k" } else { "256k" };
        let map = format!("{}:a:0", i + 1);
        let codec = format!("-c:a:{i}");
        let rate = format!("-b:a:{i}");
        cmd.args(["-map", &map, &codec, "aac", &rate, bitrate]);
    }
    cmd.args(["-c:v", "copy", "-t"]).arg(format!("{secs:.6}"));
    cmd.args(["-movflags", "+faststart"]).arg(&tmp);
    let out = cmd.stdin(Stdio::null()).output().context("failed to start ffmpeg for audio mux")?;
    if !out.status.success() {
        bail!("ffmpeg audio mux failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    std::fs::rename(&tmp, path).context("failed to move movie into place")?;
    let _ = std::fs::remove_file(&video);
    for i in 0..tracks.len() {
        let _ = std::fs::remove_file(audio_path(path, i));
    }
    Ok(())
}

/// Pops frames due at or before `t`; the newest becomes `last`. True if any was due.
fn pick(q: &mut VecDeque<(f64, Arc<Bgra>)>, last: &mut Option<Arc<Bgra>>, t: f64) -> bool {
    let mut fresh = false;
    while q.front().is_some_and(|f| f.0 <= t) {
        if let Some((_, f)) = q.pop_front() {
            *last = Some(f);
            fresh = true;
        }
    }
    fresh
}

fn queue(q: &mut VecDeque<(f64, Arc<Bgra>)>, pts: f64, frame: Arc<Bgra>) {
    q.push_back((pts, frame));
    while q.len() > QUEUE {
        q.pop_front();
    }
}

/// Records one session. Screen and camera frames arrive with clock-seconds
/// timestamps; `emit_until` places them on the fixed output grid.
pub struct Sink {
    out: Option<Out>,
    preview: Option<PreviewFn>,
    last_preview: f64,
    compositor: Option<Compositor>,
    scene: Scene,
    size: (usize, usize),
    fps: i64,
    /// Clock time of the first screen frame.
    t0: Option<f64>,
    /// Next grid slot to decide on.
    next_slot: i64,
    /// Video frames written so far (paused time excluded).
    out_slot: i64,
    pauses: Pauses,
    limit: Option<i64>,
    screen_q: VecDeque<(f64, Arc<Bgra>)>,
    cam_q: VecDeque<(f64, Arc<Bgra>)>,
    screen_last: Option<Arc<Bgra>>,
    cam_last: Option<Arc<Bgra>>,
    /// Buffers returned by the video writer.
    recycle: Receiver<Bgra>,
    /// Buffer kept when nothing is being written (preview only).
    spare: Option<Bgra>,
    /// Audio lanes: [system, mic].
    lanes: [Lane; 2],
    /// Linear mix gains: [system, mic].
    gains: [f32; 2],
    pub stats: Stats,
    failed: Option<String>,
    /// Frame whose slot is counted but that ffmpeg has not accepted yet. Kept instead of
    /// blocking on a full queue, so the sink lock is never held across a wait on ffmpeg.
    held: Option<Arc<Bgra>>,
    levels: [f32; 2],
}

impl Sink {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: Option<&Path>,
        v: &VideoSpec,
        system_audio: bool,
        mic: bool,
        layout: AudioLayout,
        gains_db: [f32; 2],
        compositor: Option<Compositor>,
        scene: Scene,
        preview: Option<PreviewFn>,
    ) -> Result<Self> {
        if let Some(p) = path {
            if p.exists() {
                bail!("{} already exists", p.display());
            }
        }
        let both = system_audio && mic;
        let mut tracks = Vec::new();
        if both && layout != AudioLayout::Stems {
            tracks.push(Track::Mix);
        }
        if !both || layout != AudioLayout::Mix {
            if system_audio {
                tracks.push(Track::Stem(AudioKind::System));
            }
            if mic {
                tracks.push(Track::Stem(AudioKind::Mic));
            }
        }
        let (recycle_tx, recycle) = mpsc::channel();
        let out = match path {
            Some(p) => Some(Out::open(p, v, &tracks, recycle_tx)?),
            None => None,
        };
        Ok(Sink {
            out,
            preview,
            last_preview: f64::NEG_INFINITY,
            scene: if compositor.is_some() { scene } else { Scene::Screen },
            compositor,
            size: (v.width, v.height),
            fps: v.fps as i64,
            t0: None,
            next_slot: 0,
            out_slot: 0,
            pauses: Pauses::default(),
            limit: None,
            screen_q: VecDeque::new(),
            cam_q: VecDeque::new(),
            screen_last: None,
            cam_last: None,
            recycle,
            spare: None,
            lanes: Default::default(),
            gains: gains_db.map(|db| 10f32.powf(db / 20.0)),
            stats: Stats::default(),
            failed: None,
            held: None,
            levels: [0.0; 2],
        })
    }

    /// Peak levels [system, mic] since the last call (linear, 0..1).
    pub fn take_levels(&mut self) -> [f32; 2] {
        std::mem::take(&mut self.levels)
    }

    /// Seconds the preview should lag behind the clock, so overlays stay in sync with audio.
    pub fn lag(&self) -> f64 {
        let worst = self.stats.screen_latency_max.max(self.stats.cam_latency_max);
        (worst + 0.025).clamp(LAG_SECS, 0.3)
    }

    pub fn started(&self) -> bool {
        self.t0.is_some()
    }

    /// Clock time (seconds) of the first frame, once started.
    pub fn t0_secs(&self) -> Option<f64> {
        self.t0
    }

    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// Switches the scene. Without a compositor only `Screen` is possible; returns false otherwise.
    pub fn set_scene(&mut self, scene: Scene) -> bool {
        if self.compositor.is_none() && scene != Scene::Screen {
            return false;
        }
        self.scene = scene;
        true
    }

    fn slot_at(&self, t: f64) -> i64 {
        match self.t0 {
            Some(t0) => ((t - t0) * self.fps as f64).ceil() as i64,
            None => 0,
        }
    }

    /// Starts a paused span at `now`. Frames are dropped until `resume`.
    pub fn pause(&mut self, now: f64) -> bool {
        if self.t0.is_none() {
            return false;
        }
        let slot = self.slot_at(now).max(self.next_slot);
        self.pauses.pause(slot)
    }

    pub fn resume(&mut self, now: f64) -> bool {
        if self.t0.is_none() {
            return false;
        }
        let slot = self.slot_at(now).max(self.next_slot);
        self.pauses.resume(slot)
    }

    pub fn is_paused(&self) -> bool {
        self.pauses.is_paused()
    }

    /// Write at most `secs` of output (rounded to whole frames).
    pub fn limit_secs(&mut self, secs: Option<f64>) {
        self.limit = secs.map(|d| (d * self.fps as f64).round() as i64);
    }

    /// Seconds of output written so far (excludes paused time).
    pub fn output_secs(&self) -> f64 {
        self.out_slot as f64 / self.fps as f64
    }

    /// Queues a screen frame. The first frame sets the timeline origin.
    pub fn push_screen(&mut self, pts: f64, frame: Arc<Bgra>) {
        self.stats.screen_frames_in += 1;
        let lat = clock_secs() - pts;
        self.stats.screen_latency_max = self.stats.screen_latency_max.max(lat);
        self.stats.screen_latency_sum += lat;
        if self.t0.is_none() {
            self.t0 = Some(pts);
        }
        queue(&mut self.screen_q, pts, frame);
    }

    pub fn push_cam(&mut self, pts: f64, frame: Arc<Bgra>) {
        self.stats.cam_frames_in += 1;
        let lat = clock_secs() - pts;
        self.stats.cam_latency_max = self.stats.cam_latency_max.max(lat);
        self.stats.cam_latency_sum += lat;
        queue(&mut self.cam_q, pts, frame);
    }

    /// Adds audio captured at clock time `pts`. Samples before the first frame are dropped.
    pub fn push_audio(&mut self, kind: AudioKind, pts: f64, frames: &[[f32; 2]]) {
        let lane = lane_of(kind);
        let peak = frames.iter().fold(0f32, |m, f| m.max(f[0].abs()).max(f[1].abs()));
        self.levels[lane] = self.levels[lane].max(peak);

        let Some(t0) = self.t0 else { return };
        let Some(out) = self.out.as_ref() else { return };
        if !out.audio.iter().any(|t| t.uses(lane)) {
            return;
        }
        let fps = self.fps as f64;
        let mut frames = frames;
        let mut start = pts;
        if start < t0 {
            let skip = ((t0 - start) * RATE).ceil() as usize;
            if skip >= frames.len() {
                return;
            }
            frames = &frames[skip..];
            start += skip as f64 / RATE;
        }
        // Real grid position of the first and last sample.
        let a = (start - t0) * fps;
        let b = a + frames.len() as f64 * fps / RATE;
        let Some((x, y, off)) = self.pauses.keep(a, b) else { return };
        let len = frames.len();
        let i0 = ((((x - a) / fps) * RATE).round() as usize).min(len);
        let i1 = ((((y - a) / fps) * RATE).round() as usize).min(len);
        if i1 <= i0 {
            return;
        }
        // Output sample index: real time minus the paused time before it.
        let real = ((start + i0 as f64 / RATE - t0) * RATE).round() as i64;
        let idx = real - (off as f64 * RATE / fps).round() as i64;
        self.lanes[lane].push(idx, &frames[i0..i1]);
    }

    /// Writes (and previews) grid slots due at or before `until`, stopping after
    /// `max_writes` video writes so callers can release the sink lock between frames.
    /// Returns true if more slots are still due.
    pub fn emit_until(&mut self, until: f64, max_writes: i64) -> bool {
        let Some(t0) = self.t0 else { return false };
        let fps = self.fps as f64;
        let half = 0.5 / fps;
        let first = self.out_slot;
        let mut more = false;
        if let Some(frame) = self.held.take() {
            self.send_video(frame);
        }
        while self.failed.is_none() && self.held.is_none() {
            let slot = self.next_slot;
            let slot_t = t0 + slot as f64 / fps;
            if slot_t > until {
                break;
            }
            if self.out_slot - first >= max_writes {
                more = true;
                break;
            }
            self.next_slot += 1;
            let fresh = pick(&mut self.screen_q, &mut self.screen_last, slot_t + half);
            pick(&mut self.cam_q, &mut self.cam_last, slot_t + half);

            let paused = self.pauses.offset_at(slot as f64).is_none();
            if paused {
                self.stats.paused_slots += 1;
            }
            let write = self.out.is_some()
                && !paused
                && self.limit.is_none_or(|l| self.out_slot < l);
            let want_preview = self.preview.is_some() && slot_t - self.last_preview >= PREVIEW_GAP;
            if !write && !want_preview {
                continue;
            }
            if self.screen_last.is_none() && self.scene != Scene::Cam {
                continue;
            }

            let (w, h) = self.size;
            let frame: Option<Arc<Bgra>> = match self.compositor.as_mut() {
                Some(c) => {
                    let mut buf = self
                        .recycle
                        .try_recv()
                        .ok()
                        .or_else(|| self.spare.take())
                        .filter(|b| b.w == w && b.h == h)
                        .unwrap_or_else(|| Bgra { w, h, px: vec![0; w * h * 4] });
                    c.render(self.scene, self.screen_last.as_deref(), self.cam_last.as_deref(), &mut buf);
                    Some(Arc::new(buf))
                }
                None => self.screen_last.clone(),
            };
            let Some(frame) = frame else { continue };
            if self.compositor.is_none() && (frame.w, frame.h) != (w, h) {
                self.failed = Some(format!(
                    "screen frame is {}x{}, expected {w}x{h}",
                    frame.w, frame.h
                ));
                break;
            }

            if want_preview {
                if let Some(p) = &self.preview {
                    p(frame.as_ref());
                }
                self.last_preview = slot_t;
            }
            if !write {
                if let Ok(buf) = Arc::try_unwrap(frame) {
                    self.spare = Some(buf);
                }
                continue;
            }
            if !fresh {
                self.stats.video_dups += 1;
            }
            self.out_slot += 1;
            self.stats.video_frames_out += 1;
            // ffmpeg interleaves inputs and will not read video past the audio it
            // has. Feed audio up to this frame before handing it over.
            self.drain(false);
            self.send_video(frame);
        }
        self.drain(false);
        more
    }

    /// Hands a frame to the video writer without blocking. A full queue keeps the
    /// frame as `held`; `emit_until` retries it on the next call.
    fn send_video(&mut self, frame: Arc<Bgra>) {
        let Some(out) = &self.out else { return };
        match out.video.try_send(frame) {
            Ok(()) => {}
            Err(TrySendError::Full(f)) => self.held = Some(f),
            Err(TrySendError::Disconnected(_)) => {
                self.failed = Some("ffmpeg stopped accepting video".into());
            }
        }
    }

    /// Lowest sample index a lane may still lack before silence is padded in. Measured
    /// in wall-clock output time, not video time: video waits on ffmpeg, and ffmpeg waits
    /// on audio, so a video-time bound would never let the first audio through.
    fn stale_limit(&self, h: i64) -> i64 {
        let Some(t0) = self.t0 else { return h - MAX_WAIT };
        if self.pauses.is_paused() {
            return h - MAX_WAIT;
        }
        let fps = self.fps as f64;
        let g = (clock_secs() - t0) * fps;
        let paused = self.pauses.offset_at(g).unwrap_or(0) as f64;
        let now_out = ((g - paused) * RATE / fps) as i64;
        now_out.max(h) - MAX_WAIT
    }

    /// Writes audio up to the video horizon. A lane that stays quiet is padded
    /// with silence after `MAX_WAIT`; `flush` writes everything at the end.
    fn drain(&mut self, flush: bool) {
        let h = self.out_slot * RATE_I / self.fps;
        let stale = self.stale_limit(h);
        let Some(out) = self.out.as_mut() else { return };
        let [n0, n1] = [self.lanes[0].next, self.lanes[1].next];
        // Audio never passes the video horizon. Tracks go to files, so no lead is needed
        // for ffmpeg to take the next frame.
        let mix_to = if flush {
            h
        } else {
            h.min(n0.min(n1).max(n0.max(n1) - MAX_WAIT).max(stale))
        };
        let lanes = &self.lanes;
        let targets: Vec<i64> = out
            .audio
            .iter()
            .map(|t| match t.track {
                Track::Mix => mix_to,
                Track::Stem(_) if flush => h,
                Track::Stem(k) => h.min(lanes[lane_of(k)].next.max(stale)),
            })
            .collect();

        // Pad each lane to the furthest target that reads it.
        let mut need = [i64::MIN; 2];
        for (t, &target) in out.audio.iter().zip(&targets) {
            for (lane, slot) in need.iter_mut().enumerate() {
                if t.uses(lane) {
                    *slot = (*slot).max(target);
                }
            }
        }
        for (lane, &n) in need.iter().enumerate() {
            if n != i64::MIN {
                self.lanes[lane].pad_to(n);
            }
        }

        let g = self.gains;
        let mut clipped = 0u64;
        for (t, &target) in out.audio.iter_mut().zip(&targets) {
            if target <= t.pos {
                continue;
            }
            let mono = t.mono();
            let mut buf: Vec<u8> = Vec::with_capacity((target - t.pos) as usize * 8);
            for j in t.pos..target {
                match t.track {
                    Track::Mix => {
                        let a = self.lanes[0].get(j);
                        let b = self.lanes[1].get(j);
                        for c in 0..2 {
                            let v = a[c] * g[0] + b[c] * g[1];
                            let v = if v.abs() > 1.0 {
                                clipped += 1;
                                v.clamp(-1.0, 1.0)
                            } else {
                                v
                            };
                            buf.extend_from_slice(&v.to_le_bytes());
                        }
                    }
                    Track::Stem(k) => {
                        let s = self.lanes[lane_of(k)].get(j);
                        if mono {
                            buf.extend_from_slice(&((s[0] + s[1]) * 0.5).to_le_bytes());
                        } else {
                            buf.extend_from_slice(&s[0].to_le_bytes());
                            buf.extend_from_slice(&s[1].to_le_bytes());
                        }
                    }
                }
            }
            match t.track {
                Track::Mix => self.stats.audio_mix_buffers += 1,
                Track::Stem(AudioKind::System) => self.stats.audio_sys_buffers += 1,
                Track::Stem(AudioKind::Mic) => self.stats.audio_mic_buffers += 1,
            }
            if t.tx.send(buf).is_err() {
                self.failed = Some("audio writer stopped".into());
                return;
            }
            t.pos = target;
        }
        self.stats.audio_clipped_samples += clipped;

        // Drop samples every track has already written.
        for lane in 0..2 {
            let keep = out
                .audio
                .iter()
                .filter(|t| t.uses(lane))
                .map(|t| t.pos)
                .min()
                .unwrap_or(i64::MAX);
            let next = self.lanes[lane].next;
            self.lanes[lane].trim(keep.min(next));
        }
    }

    fn close(&mut self) -> Result<()> {
        let secs = self.output_secs();
        match self.out.take() {
            Some(o) => o.close(secs),
            None => Ok(()),
        }
    }

    /// Writes the remaining audio and video up to `end`, finalizes the file,
    /// and returns the stats. Without any screen frame nothing is written.
    pub fn finish(mut self, end: f64) -> Result<Stats> {
        if self.out.is_none() {
            return Ok(std::mem::take(&mut self.stats));
        }
        if self.t0.is_none() {
            let _ = self.close();
            bail!("no screen frames were captured, nothing written");
        }
        self.preview = None;
        loop {
            self.emit_until(end, i64::MAX);
            if self.failed.is_some() || self.held.is_none() {
                break;
            }
            // ffmpeg is behind: keep feeding it audio while it makes room.
            self.drain(false);
            std::thread::sleep(Duration::from_millis(2));
        }
        self.drain(true);
        let closed = self.close();
        if let Some(e) = self.failed.take() {
            return match closed {
                Err(c) => Err(anyhow!("writing failed: {e}: {c:#}")),
                Ok(()) => bail!("writing failed: {e}"),
            };
        }
        closed?;
        self.stats.output_secs = self.output_secs();
        Ok(std::mem::take(&mut self.stats))
    }
}

impl Drop for Sink {
    /// Abandoned sessions still finalize the file rather than orphaning ffmpeg.
    fn drop(&mut self) {
        let _ = self.close();
    }
}
