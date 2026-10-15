//! Platform-neutral frame access on top of `VideoReader`.

use std::path::Path;

use anyhow::Result;

use super::reader::{Frame, VideoReader};
use crate::util::autoreleasepool;

/// Random-access frame lookup on top of a sequential reader: cheap when
/// times move forward, re-seeks on jumps.
pub struct FrameSource {
    reader: VideoReader,
    current: Option<Frame>,
    upcoming: Option<Frame>,
}

impl FrameSource {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(FrameSource { reader: VideoReader::open(path)?, current: None, upcoming: None })
    }

    pub fn size(&self) -> (u32, u32) {
        self.reader.size
    }

    pub fn duration_ms(&self) -> f64 {
        self.reader.duration_ms
    }

    /// The frame on screen at `t_ms` (the latest frame with pts <= t).
    pub fn frame_at(&mut self, t_ms: f64) -> Option<&Frame> {
        let half = 0.5 * 1000.0 / self.reader.fps;
        let jump = match &self.current {
            Some(c) => t_ms + half < c.pts_ms || t_ms > c.pts_ms + 1500.0,
            None => self.upcoming.is_none(),
        };
        if jump {
            self.reader.seek(t_ms).ok()?;
            self.current = None;
            self.upcoming = self.reader.next_frame();
        }
        while self.upcoming.as_ref().is_some_and(|u| u.pts_ms <= t_ms + half) {
            self.current = self.upcoming.take();
            self.upcoming = self.reader.next_frame();
        }
        if self.current.is_none() {
            // Before the first frame: show the first one.
            self.current = self.upcoming.take();
            self.upcoming = self.reader.next_frame();
        }
        self.current.as_ref()
    }
}

/// Decodes the frames for a known list of times on a background thread,
/// so hardware decode overlaps rendering and encoding.
pub struct Prefetch {
    rx: std::sync::mpsc::Receiver<(usize, Option<Frame>)>,
    _thread: std::thread::JoinHandle<()>,
}

impl Prefetch {
    pub fn spawn(path: std::path::PathBuf, times: Vec<f64>) -> Result<Self> {
        // Open eagerly so errors surface here.
        let mut src = FrameSource::open(&path)?;
        let (tx, rx) = std::sync::mpsc::sync_channel(6);
        let thread = std::thread::spawn(move || {
            for (i, t) in times.into_iter().enumerate() {
                let f = autoreleasepool(|| src.frame_at(t).cloned());
                if tx.send((i, f)).is_err() {
                    break;
                }
            }
        });
        Ok(Prefetch { rx, _thread: thread })
    }

    /// The frame for output index `i` (indices must be requested in order).
    pub fn get(&self, i: usize) -> Option<Frame> {
        loop {
            let (j, f) = self.rx.recv().ok()?;
            if j == i {
                return f;
            }
            if j > i {
                return None;
            }
        }
    }
}
