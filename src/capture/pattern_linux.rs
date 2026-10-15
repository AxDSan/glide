//! Synthetic "screen": a generated moving test pattern on the monotonic clock.
//! Used for self-tests and encoder stress tests without screen access.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::capture::record::Shared;
use crate::capture::types::{AudioKind, Bgra, clock_secs};

/// Sample rate of the synthetic audio (the audio pipeline's fixed rate).
const RATE: i64 = 48_000;

/// Length of the beep that starts with every white flash.
pub const BEEP_SECS: f64 = 0.04;

pub struct Synth {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

/// Samples [start, start+n) of the synthetic "system audio": a 1 kHz beep at
/// the start of every second (when the video flashes), silence otherwise.
/// Interleaved stereo.
pub fn beep_samples(start: i64, n: usize) -> Vec<f32> {
    let len = (BEEP_SECS * RATE as f64) as i64;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n as i64 {
        let in_sec = (start + i).rem_euclid(RATE);
        let v = if in_sec < len {
            0.5 * (2.0 * std::f64::consts::PI * 1000.0 * in_sec as f64 / RATE as f64).sin() as f32
        } else {
            0.0
        };
        out.push(v);
        out.push(v);
    }
    out
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Draw frame `n` into packed BGRA. Every `fps` frames (once per second) the
/// frame is solid white so A/V offsets can be measured against an audio click.
fn draw(f: &mut Bgra, n: u64, fps: u64, noise: bool, rng: &mut Rng) {
    let (w, h) = (f.w, f.h);
    let flash = n % fps == 0;
    let box_x = ((n * 12) as usize) % w.saturating_sub(200).max(1);
    let box_y = h / 3;
    for (y, row) in f.px.chunks_exact_mut(w * 4).enumerate() {
        for (x, p) in row.chunks_exact_mut(4).enumerate() {
            let in_box = x >= box_x && x < box_x + 200 && y >= box_y && y < box_y + 200;
            if flash || in_box {
                p.copy_from_slice(&[0xff, 0xff, 0xff, 0xff]);
            } else {
                let block = 32usize;
                let nz = if noise && (x / block + y / block + n as usize) % 3 == 0 {
                    (rng.next() & 0xff) as u8
                } else {
                    0
                };
                let g = ((x + n as usize * 4) % 256) as u8;
                p[0] = 120 ^ nz;
                p[1] = (y * 255 / h) as u8;
                p[2] = g ^ nz;
                p[3] = 0xff;
            }
        }
    }
}

impl Synth {
    /// Start producing `w`x`h` frames at `fps` (`_pix` is ignored on Linux).
    /// `audio`: also generate system audio with a beep on every flash.
    pub fn start(w: usize, h: usize, fps: u32, _pix: u32, noise: bool, audio: bool, shared: Arc<Shared>) -> Synth {
        let (w, h, fps) = (w.max(1), h.max(1), fps.max(1));
        let stop = Arc::new(AtomicBool::new(false));
        // Video and audio share this origin, so flash k and beep k both land
        // at start + k seconds.
        let start = clock_secs().ceil();
        let mut threads = vec![];
        if audio {
            let (stop_a, shared_a) = (stop.clone(), shared.clone());
            threads.push(std::thread::spawn(move || {
                let chunk = 480usize; // 10 ms
                let mut k = 0i64;
                while !stop_a.load(Ordering::SeqCst) && !shared_a.stopped() {
                    let t = start + (k * chunk as i64) as f64 / RATE as f64;
                    // Deliver each chunk once it has "played", like a real device.
                    let ready = t + chunk as f64 / RATE as f64;
                    let now = clock_secs();
                    if ready > now {
                        std::thread::sleep(Duration::from_secs_f64(ready - now));
                    }
                    let frames: Vec<[f32; 2]> = beep_samples(k * chunk as i64, chunk)
                        .chunks_exact(2)
                        .map(|s| [s[0], s[1]])
                        .collect();
                    shared_a.push_audio(AudioKind::System, t, frames);
                    k += 1;
                }
            }));
        }
        let (stop_v, shared_v) = (stop.clone(), shared);
        threads.push(std::thread::spawn(move || {
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
            let period = 1.0 / fps as f64;
            let mut n = 0u64;
            while !stop_v.load(Ordering::SeqCst) && !shared_v.stopped() {
                let due = start + n as f64 * period;
                let now = clock_secs();
                if due > now {
                    std::thread::sleep(Duration::from_secs_f64(due - now));
                }
                let mut frame = Bgra { w, h, px: vec![0; w * h * 4] };
                draw(&mut frame, n, fps as u64, noise, &mut rng);
                shared_v.push_screen(due, frame);
                n += 1;
            }
        }));
        Synth { stop, threads }
    }

    /// Stop the generator threads and wait for them to exit. Idempotent.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Synth {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beep_starts_on_each_second() {
        // Frames 0 and 1 (samples 47998, 47999) are before the second: silent.
        let s = beep_samples(RATE - 2, 2);
        assert_eq!(s, vec![0.0; 4]);
        // Sample 48000 is the second's zero-phase start; 48001 is past it.
        let s = beep_samples(RATE, 2);
        assert_eq!(s[0], 0.0);
        assert!(s[2] > 0.0);
        // Past the beep length, silence.
        let len = (BEEP_SECS * RATE as f64) as usize;
        assert!(beep_samples(RATE + len as i64, 1).iter().all(|&v| v == 0.0));
    }
}
