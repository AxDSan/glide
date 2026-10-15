//! Platform-neutral recording types shared by every capture backend: codec
//! and audio layout choices, writer statistics, and pause bookkeeping on the
//! frame grid.

pub const TIMESCALE: i32 = 60000;

/// Audio sample rate of every capture and mix stream (Hz).
pub const RATE: f64 = 48000.0;

/// Linux: one tightly packed BGRA frame (stride = w * 4).
#[cfg(target_os = "linux")]
pub struct Bgra {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

/// Linux: CLOCK_MONOTONIC seconds. Every Linux capture timestamp uses this.
#[cfg(target_os = "linux")]
pub fn clock_secs() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Codec {
    H264,
    Hevc,
    /// Apple ProRes 422 (intra-frame, large files, smoothest editing).
    Prores,
}

#[derive(Clone, Debug)]
pub struct VideoSpec {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub codec: Codec,
    pub bitrate: u64,
    /// Constant quality 0..1 instead of `bitrate` (H.264/HEVC).
    pub quality: Option<f64>,
    /// Pixel format of buffers handed to the encoder.
    pub pixel_format: u32,
}

/// Audio track layout when both system audio and mic are recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioLayout {
    /// Track 1 = system+mic mix (what players use), tracks 2-3 = system and mic
    /// stems for editors. QuickTime plays only the mix.
    Both,
    /// Only the system+mic mix (like OBS's default single track).
    Mix,
    /// Separate system and mic tracks only.
    Stems,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioKind {
    System,
    Mic,
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub screen_frames_in: u64,
    pub cam_frames_in: u64,
    pub video_frames_out: u64,
    pub video_dups: u64,
    pub video_drops: u64,
    pub audio_sys_buffers: u64,
    pub audio_mic_buffers: u64,
    pub audio_drops: u64,
    pub audio_mix_buffers: u64,
    pub audio_clipped_samples: u64,
    /// Grid slots skipped while paused.
    pub paused_slots: u64,
    /// Written screen length in seconds, known only after the writer finishes (Linux).
    pub output_secs: f64,
    /// Arrival latency (host now - capture pts), max and sum, in seconds.
    pub screen_latency_max: f64,
    pub screen_latency_sum: f64,
    pub cam_latency_max: f64,
    pub cam_latency_sum: f64,
}

/// Pause spans on the real frame grid, in slot indices `[pause, resume)`.
/// Everything captured inside a span is dropped and everything after it is
/// shifted earlier by the span's length, so the output has no gap. Spans are
/// whole slots, which keeps the video grid exact across pauses.
#[derive(Default, Debug, Clone)]
pub struct Pauses {
    spans: Vec<(i64, Option<i64>)>,
}

impl Pauses {
    pub fn is_paused(&self) -> bool {
        self.spans.last().is_some_and(|s| s.1.is_none())
    }

    /// Pause from `slot` on. Returns false if already paused.
    pub fn pause(&mut self, slot: i64) -> bool {
        if self.is_paused() {
            return false;
        }
        let floor = self.spans.last().and_then(|s| s.1).unwrap_or(0);
        self.spans.push((slot.max(floor), None));
        true
    }

    /// Resume at `slot`. Returns false if not paused.
    pub fn resume(&mut self, slot: i64) -> bool {
        let Some(last) = self.spans.last_mut() else { return false };
        if last.1.is_some() {
            return false;
        }
        if slot <= last.0 {
            self.spans.pop(); // resumed before the pause took effect
        } else {
            last.1 = Some(slot);
        }
        true
    }

    /// Slots to shift a capture at real grid position `slot` (fractional for
    /// audio), or None if it falls inside a pause and must be dropped.
    pub fn offset_at(&self, slot: f64) -> Option<i64> {
        let mut off = 0;
        for &(p, r) in &self.spans {
            if slot < p as f64 {
                break;
            }
            match r {
                Some(r) if slot >= r as f64 => off += r - p,
                _ => return None,
            }
        }
        Some(off)
    }

    /// The longest part of a capture spanning real slots `[a, b)` that lies
    /// outside pauses: `(from, to, offset)`. None if it is entirely paused.
    pub fn keep(&self, a: f64, b: f64) -> Option<(f64, f64, i64)> {
        let mut best: Option<(f64, f64, i64)> = None;
        let mut from = f64::NEG_INFINITY;
        let mut off = 0;
        let mut consider = |lo: f64, hi: f64, off: i64| {
            let (x, y) = (lo.max(a), hi.min(b));
            if y > x && best.is_none_or(|(p, q, _)| y - x > q - p) {
                best = Some((x, y, off));
            }
        };
        for &(p, r) in &self.spans {
            consider(from, p as f64, off);
            match r {
                Some(r) => {
                    off += r - p;
                    from = r as f64;
                }
                None => return best,
            }
        }
        consider(from, f64::INFINITY, off);
        best
    }

    /// Total paused slots (closed spans only).
    #[cfg(test)]
    pub fn total(&self) -> i64 {
        self.spans.iter().filter_map(|&(p, r)| r.map(|r| r - p)).sum()
    }
}
