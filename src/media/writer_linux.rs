//! Linux movie writer: BGRA frames stream into ffmpeg. The soundtrack, if
//! any, is written to a temp file and muxed in a second pass at `finish`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};

use super::ffmpeg::{self, AUDIO_RATE, VideoKind};
use crate::project::model::Codec;
use crate::render::VideoFrame;

pub struct WriterSpec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: Codec,
    /// Average bitrate, used when `quality` is None (fit to a file size).
    pub bitrate: u64,
    /// 0..1 quality (higher is better); None uses `bitrate`.
    pub quality: Option<f64>,
    pub audio: bool,
}

pub struct MovieWriter {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// Video-only pass; the final movie is this plus the soundtrack.
    video: PathBuf,
    out: PathBuf,
    width: u32,
    height: u32,
    fps: u32,
    soundtrack: Option<Arc<Vec<[f32; 2]>>>,
    pub frames: u64,
}

fn kind(codec: Codec) -> VideoKind {
    match codec {
        Codec::H264 => VideoKind::H264,
        Codec::Hevc => VideoKind::Hevc,
        Codec::Prores => VideoKind::Prores4444,
    }
}

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|x| x.to_string()).collect()
}

impl MovieWriter {
    pub fn create(path: &Path, s: &WriterSpec) -> Result<Self> {
        if path.exists() {
            std::fs::remove_file(path).with_context(|| format!("replacing {}", path.display()))?;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
        let video = path.with_extension(format!("video.{ext}"));
        let _ = std::fs::remove_file(&video);
        let kind = kind(s.codec);
        let size = format!("{}x{}", s.width, s.height);
        let fps = s.fps.to_string();

        let mut args = strings(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "-s",
        ]);
        args.push(size);
        args.push("-r".into());
        args.push(fps);
        args.extend(strings(&["-i", "pipe:0", "-vf"]));
        args.push(ffmpeg::video_filter(kind).to_string());
        args.extend(ffmpeg::encoder_args(kind, s.quality, s.bitrate, s.fps));
        args.extend(strings(&[
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "iec61966-2-1",
            "-movflags",
            "+faststart",
        ]));
        args.push(ffmpeg::utf8(&video)?.to_string());

        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting ffmpeg (is it installed?)")?;
        let stdin = child.stdin.take();
        Ok(MovieWriter {
            child: Some(child),
            stdin,
            video,
            out: path.to_path_buf(),
            width: s.width,
            height: s.height,
            fps: s.fps,
            soundtrack: None,
            frames: 0,
        })
    }

    /// Interleaved stereo at 48 kHz to write alongside the frames.
    pub fn set_soundtrack(&mut self, mix: Arc<Vec<[f32; 2]>>) {
        self.soundtrack = Some(mix);
    }

    /// Append one BGRA frame of exactly the writer's size.
    pub fn push_frame(&mut self, f: &VideoFrame) -> Result<()> {
        if f.width != self.width || f.height != self.height {
            bail!("frame is {}x{}, movie is {}x{}", f.width, f.height, self.width, self.height);
        }
        let row = self.width as usize * 4;
        let stdin = self.stdin.as_mut().context("movie already finished")?;
        let wrote = (0..self.height as usize).try_for_each(|y| {
            let at = y * f.stride;
            stdin.write_all(&f.bgra[at..at + row])
        });
        if wrote.is_err() {
            return Err(self.failure());
        }
        self.frames += 1;
        Ok(())
    }

    /// Append one BGRA frame from CPU memory (`stride` bytes per row).
    pub fn push_bgra(&mut self, bgra: &[u8], width: u32, height: u32, stride: usize) -> Result<()> {
        self.push_frame(&VideoFrame { width, height, stride, bgra })
    }

    /// The encoder died: reap it and report why.
    fn failure(&mut self) -> anyhow::Error {
        self.stdin = None;
        match self.child.take().map(Child::wait_with_output) {
            Some(Ok(out)) => anyhow!("ffmpeg encoder failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
            _ => anyhow!("ffmpeg encoder failed"),
        }
    }

    pub fn finish(mut self) -> Result<()> {
        self.stdin = None;
        let child = self.child.take().context("movie already finished")?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("ffmpeg encoder failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let res = match self.soundtrack.take() {
            Some(mix) => self.mux_soundtrack(&mix),
            None => std::fs::rename(&self.video, &self.out).context("moving movie into place"),
        };
        let _ = std::fs::remove_file(&self.video);
        res
    }

    fn mux_soundtrack(&self, mix: &[[f32; 2]]) -> Result<()> {
        let audio = self.video.with_extension("audio.f32");
        let res = write_f32(&audio, mix).and_then(|()| {
            let seconds = format!("{:.6}", self.frames as f64 / self.fps as f64);
            let rate = AUDIO_RATE.to_string();
            ffmpeg::ffmpeg(&[
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                ffmpeg::utf8(&self.video)?,
                "-f",
                "f32le",
                "-ar",
                &rate,
                "-ac",
                "2",
                "-i",
                ffmpeg::utf8(&audio)?,
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c:v",
                "copy",
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-t",
                &seconds,
                "-movflags",
                "+faststart",
                ffmpeg::utf8(&self.out)?,
            ])
            .map(|_| ())
        });
        let _ = std::fs::remove_file(&audio);
        res
    }

    /// Abandon the movie; the partial file is removed.
    pub fn cancel(self) {
        drop(self);
    }
}

impl Drop for MovieWriter {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            self.stdin = None;
            let _ = c.kill();
            let _ = c.wait();
            let _ = std::fs::remove_file(&self.video);
        }
    }
}

fn write_f32(path: &Path, mix: &[[f32; 2]]) -> Result<()> {
    let mut w = BufWriter::new(File::create(path).with_context(|| format!("writing {}", path.display()))?);
    for s in mix {
        w.write_all(&s[0].to_le_bytes())?;
        w.write_all(&s[1].to_le_bytes())?;
    }
    w.flush()?;
    Ok(())
}
