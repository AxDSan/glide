//! `FrameSource::frame_at` and `Prefetch` checked against ffmpeg's own full
//! decode of a generated clip. Linux only: the reader is the ffmpeg-CLI
//! backend. The clip has B-frames so presentation order differs from decode
//! order, and seeks go forward, backward, randomly, and across frame
//! boundaries.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

use glide::media::{FrameSource, Prefetch};

const W: u32 = 320;
const H: u32 = 240;
const FPS: u32 = 60;
const SECS: u32 = 3;
const FRAME_BYTES: usize = (W * H * 4) as usize;

/// Removes its directory when the test ends, pass or fail.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Clip {
    path: PathBuf,
    /// Full BGRA decode, `FRAME_BYTES` per frame, in presentation order.
    bgra: Vec<u8>,
    /// Presentation time of each frame in ms, ascending.
    pts_ms: Vec<f64>,
    _dir: TempDir,
}

fn run_out(prog: &str, args: &[&str]) -> Vec<u8> {
    let out = Command::new(prog).args(args).output().unwrap_or_else(|e| panic!("{prog} must be on PATH: {e}"));
    assert!(out.status.success(), "{prog} {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn ffmpeg_out(args: &[&str]) -> Vec<u8> {
    run_out("ffmpeg", args)
}

fn ffprobe_out(args: &[&str]) -> Vec<u8> {
    run_out("ffprobe", args)
}

fn make_clip(name: &str) -> Clip {
    let dir = TempDir(std::env::temp_dir().join(format!("glide-seek-{name}-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let path = dir.0.join("clip.mp4");
    let src = format!("testsrc2=size={W}x{H}:rate={FPS}");
    let secs = SECS.to_string();
    let p = path.to_str().unwrap();
    let encode: Vec<&str> = vec![
        "-v", "error", "-y", "-f", "lavfi", "-i", &src, "-t", &secs, "-c:v", "libx264", "-preset",
        "ultrafast", "-bf", "2", "-g", "30", "-pix_fmt", "yuv420p", p,
    ];
    ffmpeg_out(&encode);

    let bgra = ffmpeg_out(&["-v", "error", "-i", p, "-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", "bgra", "pipe:1"]);
    assert_eq!(bgra.len() % FRAME_BYTES, 0, "decode is not whole frames");

    let pts = ffprobe_out(&["-v", "error", "-select_streams", "v:0", "-show_entries", "frame=pts_time", "-of", "csv=p=0", p]);
    let pts_ms: Vec<f64> = String::from_utf8_lossy(&pts)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok())
        .map(|s| s * 1000.0)
        .collect();
    assert_eq!(pts_ms.len(), bgra.len() / FRAME_BYTES, "pts list and decoded frame count differ");
    assert!(pts_ms.windows(2).all(|w| w[0] < w[1]), "pts not strictly ascending");
    Clip { path, bgra, pts_ms, _dir: dir }
}

/// Index of the frame on screen at `t_ms` under the rule documented on
/// `FrameSource::frame_at`: the latest frame with pts <= t + half a frame,
/// or the first frame before the clip starts.
fn expected_index(pts_ms: &[f64], t_ms: f64) -> usize {
    let half = 0.5 * 1000.0 / FPS as f64;
    pts_ms.iter().rposition(|&p| p <= t_ms + half).unwrap_or(0)
}

/// Asserts that `frame` is exactly reference frame `k` of `clip`.
fn assert_frame(clip: &Clip, k: usize, frame: &glide::media::Frame, ctx: &str) {
    assert!(
        (frame.pts_ms - clip.pts_ms[k]).abs() < 0.5,
        "{ctx}: pts {:.3} ms, expected frame {k} at {:.3} ms",
        frame.pts_ms,
        clip.pts_ms[k]
    );
    let want = &clip.bgra[k * FRAME_BYTES..(k + 1) * FRAME_BYTES];
    frame.with_bgra(|v| {
        assert_eq!((v.width, v.height), (W, H), "{ctx}: size");
        let row = W as usize * 4;
        for y in 0..H as usize {
            let got = &v.bgra[y * v.stride..y * v.stride + row];
            let exp = &want[y * row..(y + 1) * row];
            assert!(got == exp, "{ctx}: pixels of frame {k} differ at row {y}");
        }
    });
}

fn check_seek(src: &mut FrameSource, clip: &Clip, t_ms: f64, ctx: &str) {
    let k = expected_index(&clip.pts_ms, t_ms);
    let frame = src.frame_at(t_ms).unwrap_or_else(|| panic!("{ctx}: no frame at {t_ms} ms"));
    assert_frame(clip, k, frame, &format!("{ctx} t={t_ms:.3}"));
}

/// Deterministic 64-bit LCG so failures reproduce.
struct Lcg(u64);

impl Lcg {
    fn unit(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn frame_ms(i: usize) -> f64 {
    i as f64 * 1000.0 / FPS as f64
}

#[test]
fn sanity_clip_has_expected_shape() {
    let clip = make_clip("sanity");
    assert_eq!(clip.pts_ms.len(), (SECS * FPS) as usize);
    assert!(clip.pts_ms[0].abs() < 0.5, "first pts {} not at 0", clip.pts_ms[0]);
    assert!(Path::new(&clip.path).exists());
}

#[test]
fn sequential_playback_matches_every_frame() {
    let clip = make_clip("sequential");
    let mut src = FrameSource::open(&clip.path).unwrap();
    assert_eq!(src.size(), (W, H));
    for i in 0..clip.pts_ms.len() {
        // Just after each frame's start: the frame itself must be shown.
        check_seek(&mut src, &clip, frame_ms(i) + 2.0, "sequential");
    }
}

#[test]
fn frame_boundaries_switch_at_the_half_frame_rule() {
    let clip = make_clip("boundary");
    let mut src = FrameSource::open(&clip.path).unwrap();
    // Walk up to each boundary from below and above, in order.
    for i in 1..clip.pts_ms.len() {
        let p = clip.pts_ms[i];
        check_seek(&mut src, &clip, p - 1.0, "boundary below");
        check_seek(&mut src, &clip, p + 1.0, "boundary above");
    }
}

#[test]
fn backward_seeks_return_earlier_frames() {
    let clip = make_clip("backward");
    let mut src = FrameSource::open(&clip.path).unwrap();
    let end = SECS as f64 * 1000.0;
    let mut t = end - 1.0;
    while t > 0.0 {
        check_seek(&mut src, &clip, t, "backward");
        t -= 137.0;
    }
    check_seek(&mut src, &clip, 0.0, "backward to start");
}

#[test]
fn random_seeks_match_reference() {
    let clip = make_clip("random");
    let mut src = FrameSource::open(&clip.path).unwrap();
    let mut rng = Lcg(0x5EED_1234_ABCD_0001);
    let end = SECS as f64 * 1000.0;
    for n in 0..400 {
        let t = end * rng.unit();
        check_seek(&mut src, &clip, t, &format!("random #{n}"));
    }
}

#[test]
fn prefetch_yields_requested_frames_in_order() {
    let clip = make_clip("prefetch");
    // Output timeline at the clip's rate, offset half a frame so each output
    // frame lands between two source frames.
    let times: Vec<f64> = (0..clip.pts_ms.len()).map(|i| frame_ms(i) + 4.0).collect();
    let pf = Prefetch::spawn(clip.path.clone(), times.clone()).unwrap();
    for (i, &t) in times.iter().enumerate() {
        let frame = pf.get(i).unwrap_or_else(|| panic!("prefetch: no frame for index {i}"));
        assert_frame(&clip, expected_index(&clip.pts_ms, t), &frame, &format!("prefetch i={i}"));
    }
}

#[test]
fn prefetch_with_jumps_still_in_order() {
    let clip = make_clip("prefetch-jumps");
    // Non-monotone source times are still requested in index order.
    let mut rng = Lcg(0xFEED_F00D_0000_0002);
    let end = SECS as f64 * 1000.0;
    let times: Vec<f64> = (0..200).map(|_| end * rng.unit()).collect();
    let pf = Prefetch::spawn(clip.path.clone(), times.clone()).unwrap();
    for (i, &t) in times.iter().enumerate() {
        let frame = pf.get(i).unwrap_or_else(|| panic!("prefetch jumps: no frame for index {i}"));
        assert_frame(&clip, expected_index(&clip.pts_ms, t), &frame, &format!("prefetch jumps i={i}"));
    }
}
