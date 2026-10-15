//! Voice isolation on Linux: RNNoise (`nnnoiseless`) run offline over the
//! downmixed track. Needs no system unit, so it always returns `Some`.

use nnnoiseless::DenoiseState;

use super::audio::Stereo;

/// Denoise a 48 kHz stereo track (processed as mono) and return it as stereo,
/// same length as the input.
pub fn isolate_voice(input: &[Stereo]) -> Option<Vec<Stereo>> {
    const FRAME: usize = DenoiseState::FRAME_SIZE;
    if input.is_empty() {
        return Some(vec![]);
    }
    // RNNoise works in i16 scale and its first output frame is a warm-up:
    // output frame k+1 corresponds to input frame k.
    let mono: Vec<f32> = input.iter().map(|s| (s[0] + s[1]) * 0.5 * 32768.0).collect();
    let frames = mono.len().div_ceil(FRAME) + 1;
    let mut state = DenoiseState::new();
    let mut out = Vec::with_capacity(frames * FRAME);
    let mut frame_in = [0f32; FRAME];
    let mut frame_out = [0f32; FRAME];
    for k in 0..frames {
        let start = k * FRAME;
        frame_in.fill(0.0);
        if start < mono.len() {
            let end = (start + FRAME).min(mono.len());
            frame_in[..end - start].copy_from_slice(&mono[start..end]);
        }
        state.process_frame(&mut frame_out, &frame_in);
        if k > 0 {
            out.extend_from_slice(&frame_out);
        }
    }
    Some(out[..input.len()].iter().map(|&v| {
        let v = v / 32768.0;
        [v, v]
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_keeps_length_and_reduces_steady_noise() {
        // Hiss alone: voice isolation should remove most of it.
        let mut x = 12345u32;
        let noise: Vec<Stereo> = (0..48_000 * 2)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let v = (x as f32 / u32::MAX as f32 - 0.5) * 0.1;
                [v, v]
            })
            .collect();
        let out = isolate_voice(&noise).expect("RNNoise always runs");
        assert_eq!(out.len(), noise.len());
        let rms = |b: &[Stereo]| (b.iter().map(|s| s[0] * s[0]).sum::<f32>() / b.len() as f32).sqrt();
        let (before, after) = (rms(&noise[48_000..]), rms(&out[48_000..]));
        // RNNoise is trained on speech and only partly attenuates stationary
        // white hiss (measured ~0.51x on this seeded input), so require a
        // clear reduction rather than a near-silent output.
        assert!(after < before * 0.6, "noise rms {before} -> {after}");
    }
}
