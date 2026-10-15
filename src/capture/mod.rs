//! Capture: ScreenCaptureKit screen + audio, AVFoundation camera, and the
//! constant-frame-rate AVAssetWriter sink.
//!
//! Adapted from rec-rs by Michael Shimeles (MIT OR Apache-2.0), the same
//! author's recorder; see README for credit.

#[cfg_attr(target_os = "linux", path = "camera_linux.rs")]
pub mod camera;
#[cfg_attr(target_os = "linux", path = "compose_linux.rs")]
pub mod compose;
#[cfg_attr(target_os = "linux", path = "meter_linux.rs")]
pub mod meter;
#[cfg(target_os = "linux")]
pub mod kwin;
#[cfg(target_os = "linux")]
#[path = "audio_linux.rs"]
pub mod audio;
#[cfg(target_os = "macos")]
pub mod mixer;
/// Linux: the shared sample rate only. The mixer itself is ScreenCaptureKit-specific.
#[cfg(target_os = "linux")]
pub mod mixer {
    pub use super::types::RATE;
}
#[cfg_attr(target_os = "linux", path = "pattern_linux.rs")]
pub mod pattern;
#[cfg_attr(target_os = "linux", path = "preview_linux.rs")]
pub mod preview;
#[cfg_attr(target_os = "linux", path = "record_linux.rs")]
pub mod record;
#[cfg_attr(target_os = "linux", path = "screen_linux.rs")]
pub mod screen;
#[cfg_attr(target_os = "linux", path = "tcc_linux.rs")]
pub mod tcc;
#[cfg_attr(target_os = "linux", path = "winsize_linux.rs")]
pub mod winsize;
pub mod types;
#[cfg_attr(target_os = "linux", path = "writer_linux.rs")]
pub mod writer;
