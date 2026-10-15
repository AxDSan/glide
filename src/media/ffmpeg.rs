//! ffmpeg/ffprobe subprocess helpers for the Linux media path.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Sample rate of all decoded and written audio.
pub const AUDIO_RATE: u32 = 48_000;

/// Video encoding target. Recording and export use different ProRes profiles
/// (422 vs 4444), so the profile is part of the kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoKind {
    H264,
    Hevc,
    Prores422,
    Prores4444,
}

/// Runs a tool and returns its stdout; a failure carries its stderr.
fn run(bin: &str, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to run {bin} (is ffmpeg installed?)"))?;
    if !out.status.success() {
        bail!("{bin} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Runs `ffmpeg` with the given arguments; returns stdout.
pub fn ffmpeg(args: &[&str]) -> Result<Vec<u8>> {
    run("ffmpeg", args)
}

/// Runs `ffprobe` with the given arguments; returns stdout.
pub fn ffprobe(args: &[&str]) -> Result<Vec<u8>> {
    run("ffprobe", args)
}

/// Path as UTF-8 for command-line arguments.
pub fn utf8(path: &Path) -> Result<&str> {
    path.to_str().with_context(|| format!("non-UTF-8 path {}", path.display()))
}

/// Stream facts for a media file.
pub struct Probe {
    pub width: u32,
    pub height: u32,
    /// Average frame rate of the video track (60 if unknown).
    pub fps: f64,
    /// Video track duration in ms (container duration if the track has none).
    pub duration_ms: f64,
    /// Container start time in seconds; ffmpeg `-ss` is relative to this.
    pub start_s: f64,
    pub audio_tracks: usize,
}

pub fn probe(path: &Path) -> Result<Probe> {
    let out = ffprobe(&["-v", "error", "-print_format", "json", "-show_format", "-show_streams", utf8(path)?])?;
    let v: Value = serde_json::from_slice(&out).context("ffprobe output")?;
    let streams = v["streams"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .with_context(|| format!("no video track in {}", path.display()))?;
    let num = |x: &Value| x.as_str().and_then(|s| s.parse::<f64>().ok());
    let rate = |x: &Value| {
        let (n, d) = x.as_str()?.split_once('/')?;
        let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
        (n > 0.0 && d > 0.0).then_some(n / d)
    };
    let width = video["width"].as_u64().unwrap_or(0) as u32;
    let height = video["height"].as_u64().unwrap_or(0) as u32;
    if width == 0 || height == 0 {
        bail!("video track in {} has no size", path.display());
    }
    let fps = rate(&video["avg_frame_rate"]).or_else(|| rate(&video["r_frame_rate"])).unwrap_or(60.0);
    let duration = num(&video["duration"]).or_else(|| num(&v["format"]["duration"])).unwrap_or(0.0);
    Ok(Probe {
        width,
        height,
        fps,
        duration_ms: duration * 1000.0,
        start_s: num(&v["format"]["start_time"]).unwrap_or(0.0),
        audio_tracks: streams.iter().filter(|s| s["codec_type"] == "audio").count(),
    })
}

/// Audio stream count; works for audio-only files (WAV) that `probe` rejects.
pub fn audio_tracks(path: &Path) -> Result<usize> {
    let out = ffprobe(&["-v", "error", "-print_format", "json", "-show_streams", utf8(path)?])?;
    let v: Value = serde_json::from_slice(&out).context("ffprobe output")?;
    Ok(v["streams"].as_array().map_or(0, |s| s.iter().filter(|s| s["codec_type"] == "audio").count()))
}

/// Presentation time in seconds of every video frame, ascending, absolute
/// in the file's timeline (same clock as `Probe::start_s`).
pub fn video_frame_times(path: &Path) -> Result<Vec<f64>> {
    let out = ffprobe(&[
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "packet=pts_time",
        "-of",
        "csv=p=0",
        utf8(path)?,
    ])?;
    let mut t: Vec<f64> = String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse().ok())
        .collect();
    t.sort_by(f64::total_cmp);
    Ok(t)
}

/// Whether the encoder can actually open here (NVENC needs a usable GPU and
/// driver; the mere presence in the build is not enough).
fn encoder_opens(name: &str) -> bool {
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=256x256:rate=1",
            "-frames:v",
            "1",
            "-c:v",
            name,
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `GLIDE_SOFTWARE_ENCODE=1` skips the hardware encoder (to test or work
/// around a broken driver).
fn pick(hardware: &'static str, software: &'static str) -> &'static str {
    let forced = std::env::var_os("GLIDE_SOFTWARE_ENCODE").is_some_and(|v| v != "0");
    if !forced && encoder_opens(hardware) { hardware } else { software }
}

/// ffmpeg video encoder name for a kind: NVENC when it opens, else software.
/// Probed once per process; infallible.
pub fn video_encoder(kind: VideoKind) -> &'static str {
    static H264: LazyLock<&'static str> = LazyLock::new(|| pick("h264_nvenc", "libx264"));
    static HEVC: LazyLock<&'static str> = LazyLock::new(|| pick("hevc_nvenc", "libx265"));
    match kind {
        VideoKind::H264 => *H264,
        VideoKind::Hevc => *HEVC,
        VideoKind::Prores422 | VideoKind::Prores4444 => "prores_ks",
    }
}

/// Global options that let the BGRA-to-YUV conversion run on several cores:
/// about 20% less CPU than a single thread at 1080p60. Capped at 4 so a
/// recording leaves cores for the rest of the system.
pub fn filter_threads() -> [String; 2] {
    let n = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 4);
    ["-filter_threads".into(), n.to_string()]
}

/// Filter chain from BGRA input to the encoder's pixel format, converting
/// with the BT.709 matrix used by the macOS writer.
pub fn video_filter(kind: VideoKind) -> &'static str {
    match kind {
        VideoKind::Prores422 | VideoKind::Prores4444 => "scale=out_color_matrix=bt709:out_range=tv,format=yuv444p10le",
        _ => "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p",
    }
}

/// Encoder arguments: `-c:v`, GOP (2 s), preset, and rate control.
/// `quality` (0..1, higher is better) maps to CRF/CQ 28 (q 0.5) .. 18 (q 1);
/// otherwise `bitrate` (bits/s) is used. ProRes ignores both (fixed quality).
/// `realtime` (recording) must keep up with the capture: without a hardware
/// encoder it uses H.264 at the fastest preset, since libx265 or libx264 at
/// `medium` cannot encode 1080p60 live on a modest CPU.
pub fn encoder_args(kind: VideoKind, quality: Option<f64>, bitrate: u64, fps: u32, realtime: bool) -> Vec<String> {
    let mut enc = video_encoder(kind);
    let mut kind = kind;
    let software = !enc.ends_with("_nvenc");
    if realtime && software && matches!(kind, VideoKind::H264 | VideoKind::Hevc) {
        (enc, kind) = ("libx264", VideoKind::H264);
    }
    let mut a = Vec::new();
    pair(&mut a, "-c:v", enc);
    pair(&mut a, "-g", fps.max(1) * 2);
    match kind {
        VideoKind::Prores422 => pair(&mut a, "-profile:v", "2"),
        VideoKind::Prores4444 => pair(&mut a, "-profile:v", "4444"),
        VideoKind::H264 | VideoKind::Hevc => {
            let hw = enc.ends_with("_nvenc");
            pair(&mut a, "-preset", if hw { "p5" } else if realtime { "ultrafast" } else { "medium" });
            if kind == VideoKind::Hevc {
                // Apple players only accept HEVC tagged hvc1.
                pair(&mut a, "-tag:v", "hvc1");
            }
            match quality {
                Some(q) => {
                    let crf = (28.0 - (q.clamp(0.0, 1.0) - 0.5) * 20.0).round() as i64;
                    if hw {
                        pair(&mut a, "-rc", "vbr");
                        pair(&mut a, "-cq", crf);
                        pair(&mut a, "-b:v", 0);
                    } else {
                        pair(&mut a, "-crf", crf);
                    }
                }
                None => pair(&mut a, "-b:v", bitrate),
            }
        }
    }
    a
}

fn pair(a: &mut Vec<String>, key: &str, value: impl ToString) {
    a.push(key.to_string());
    a.push(value.to_string());
}
