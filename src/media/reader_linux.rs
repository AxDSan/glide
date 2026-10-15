//! Linux media reader: frame times from ffprobe, pixels from an ffmpeg
//! rawvideo BGRA pipe started at each seek point.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use super::ffmpeg::{self, AUDIO_RATE};
use crate::render::VideoFrame;

/// One decoded video frame, BGRA rows of `stride` bytes.
#[derive(Clone)]
pub struct Frame {
    pub pts_ms: f64,
    width: u32,
    height: u32,
    stride: usize,
    data: Arc<[u8]>,
}

impl Frame {
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Hand the pixels to `f` as BGRA.
    pub fn with_bgra<R>(&self, f: impl FnOnce(&VideoFrame) -> R) -> R {
        f(&VideoFrame { width: self.width, height: self.height, stride: self.stride, bgra: &self.data })
    }
}

/// Sequential video reader that can restart at any time.
pub struct VideoReader {
    path: PathBuf,
    pub size: (u32, u32),
    pub fps: f64,
    pub duration_ms: f64,
    /// Absolute frame presentation times in seconds, ascending.
    times: Vec<f64>,
    /// Frame 0's time; `pts_ms` is relative to it (as on macOS).
    origin_s: f64,
    /// Container start; ffmpeg `-ss` is relative to this.
    start_s: f64,
    /// Index into `times` of the next frame the pipe will produce.
    next: usize,
    child: Option<Child>,
    out: Option<ChildStdout>,
}

impl VideoReader {
    pub fn open(path: &Path) -> Result<Self> {
        let info = ffmpeg::probe(path)?;
        let times = ffmpeg::video_frame_times(path)?;
        let origin_s = *times.first().with_context(|| format!("no video frames in {}", path.display()))?;
        let mut r = VideoReader {
            path: path.to_path_buf(),
            size: (info.width, info.height),
            fps: info.fps,
            duration_ms: info.duration_ms,
            times,
            origin_s,
            start_s: info.start_s,
            next: 0,
            child: None,
            out: None,
        };
        r.seek(0.0)?;
        Ok(r)
    }

    /// Restart decoding so the next frame is the one shown at `t_ms`.
    pub fn seek(&mut self, t_ms: f64) -> Result<()> {
        self.stop();
        // One frame early, so the frame that starts at `t_ms` is included.
        let want = self.origin_s + (t_ms - 1000.0 / self.fps).max(0.0) / 1000.0;
        let idx = self.times.partition_point(|&t| t <= want).saturating_sub(1);
        let ss = format!("{:.6}", self.times[idx] - self.start_s);
        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-ss",
                &ss,
                "-i",
                ffmpeg::utf8(&self.path)?,
                "-map",
                "0:v:0",
                "-an",
                "-sn",
                "-fps_mode",
                "passthrough",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "bgra",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("starting ffmpeg (is it installed?)")?;
        self.out = child.stdout.take();
        self.child = Some(child);
        self.next = idx;
        Ok(())
    }

    pub fn next_frame(&mut self) -> Option<Frame> {
        let pts_s = *self.times.get(self.next)?;
        let (width, height) = self.size;
        let stride = width as usize * 4;
        let mut data = vec![0u8; stride * height as usize];
        self.out.as_mut()?.read_exact(&mut data).ok()?;
        self.next += 1;
        Some(Frame {
            pts_ms: (pts_s - self.origin_s) * 1000.0,
            width,
            height,
            stride,
            data: data.into(),
        })
    }

    fn stop(&mut self) {
        self.out = None;
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for VideoReader {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Decode an audio track to 48 kHz interleaved stereo.
pub fn read_audio(path: &Path, track_index: usize) -> Result<Vec<[f32; 2]>> {
    let tracks = ffmpeg::audio_tracks(path)?;
    if track_index >= tracks {
        bail!("no audio track {track_index} in {}", path.display());
    }
    let map = format!("0:a:{track_index}");
    let rate = AUDIO_RATE.to_string();
    // aresample=async=1: ffmpeg's timestamp-compensating resample mode. Behaviour on gapped audio is unverified.
    let raw = ffmpeg::ffmpeg(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-i",
        ffmpeg::utf8(path)?,
        "-map",
        &map,
        "-af",
        "aresample=async=1",
        "-ac",
        "2",
        "-ar",
        &rate,
        "-f",
        "f32le",
        "pipe:1",
    ])?;
    Ok(raw
        .chunks_exact(8)
        .map(|c| {
            [
                f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            ]
        })
        .collect())
}

/// Number of audio tracks in a file.
pub fn audio_track_count(path: &Path) -> Result<usize> {
    ffmpeg::audio_tracks(path).context("opening media")
}
