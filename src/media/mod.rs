//! Recorded media: frame reading, audio decode, movie writing.
//! macOS uses AVFoundation; Linux drives ffmpeg (see `ffmpeg`).

#[cfg_attr(target_os = "linux", path = "reader_linux.rs")]
mod reader;
mod source;
#[cfg_attr(target_os = "linux", path = "writer_linux.rs")]
pub mod writer;
#[cfg(target_os = "linux")]
pub mod ffmpeg;

pub use reader::{Frame, VideoReader, audio_track_count, read_audio};
pub use source::{FrameSource, Prefetch};
