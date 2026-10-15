//! Permission status on Linux. There is no TCC database: the desktop portals
//! ask at the moment a capture starts (ScreenCast on the first record, the
//! PipeWire session for audio), so nothing can be requested up front.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Granted,
    Denied,
    NotDetermined,
    Restricted,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Status::Granted => "granted",
            Status::Denied => "DENIED",
            Status::NotDetermined => "not yet requested",
            Status::Restricted => "restricted",
        })
    }
}

/// macOS only: re-execs so TCC attributes access to `rec`. A no-op here.
pub fn disclaim_and_reexec() {}

pub fn screen_status() -> Status {
    Status::Granted
}

pub fn camera_status() -> Status {
    Status::Granted
}

pub fn mic_status() -> Status {
    Status::Granted
}

/// Portal prompts appear when a capture starts, so there is nothing to request here.
pub fn request_all(_screen: bool, _mic: bool, _camera: bool, _wait: Duration) {}
