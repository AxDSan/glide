//! Window sizing through KWin scripting. Window ids are the `screen::key32`
//! hash of the KWin uuid, so they stay stable for the window's life.

use anyhow::{Result, anyhow};

use super::{kwin, screen};

/// Common recording sizes (points), grouped like a size menu.
pub const PRESETS: &[(&str, &[(u32, u32)])] = &[
    ("16:9", &[(1280, 720), (1920, 1080), (2560, 1440)]),
    ("16:10", &[(1280, 800), (1440, 900), (1680, 1050), (1920, 1200)]),
    ("4:3", &[(800, 600), (1024, 768), (1280, 960), (1600, 1200)]),
    ("9:16", &[(720, 1280), (900, 1600), (1080, 1920)]),
    ("Square", &[(800, 800), (1080, 1080), (1280, 1280)]),
];

/// KWin scripting needs no accessibility grant.
pub fn trusted(_prompt: bool) -> bool {
    true
}

fn find(id: u32) -> Result<kwin::KWindow> {
    kwin::windows()?
        .into_iter()
        .find(|w| screen::key32(&w.id) == id)
        .ok_or_else(|| anyhow!("window {id} is not on screen"))
}

/// The frontmost listed window of process `pid`.
pub fn window_of_pid(pid: i32) -> Option<u32> {
    let pid = u32::try_from(pid).ok()?;
    kwin::windows()
        .ok()?
        .into_iter()
        .find(|w| w.pid == pid)
        .map(|w| screen::key32(&w.id))
}

/// Size (logical px) of window `id`, frame included.
pub fn window_size(id: u32) -> Result<(f64, f64)> {
    let w = find(id)?;
    Ok((w.rect[2], w.rect[3]))
}

/// Size (logical px) of the output showing each window; `f64::MAX` when unknown.
pub fn display_sizes(windows: &[screen::WindowInfo]) -> Vec<(f64, f64)> {
    let listed = kwin::windows().unwrap_or_default();
    let outputs = kwin::outputs().unwrap_or_default();
    windows
        .iter()
        .map(|w| {
            listed
                .iter()
                .find(|k| screen::key32(&k.id) == w.id)
                .and_then(|k| outputs.iter().find(|o| o.name == k.output))
                .map_or((f64::MAX, f64::MAX), |o| (o.rect[2], o.rect[3]))
        })
        .collect()
}

/// Listed window ids, frontmost first.
pub fn front_to_back() -> Vec<u32> {
    kwin::windows()
        .map(|ws| ws.iter().map(|w| screen::key32(&w.id)).collect())
        .unwrap_or_default()
}

/// Activates window `id` and raises it.
pub fn raise(id: u32) -> Result<()> {
    kwin::raise(&find(id)?.id)
}

/// Resizes window `id` to `width`×`height` logical px, raises it, and returns
/// the size KWin accepted (apps and outputs can clamp it).
pub fn resize(id: u32, width: f64, height: f64) -> Result<(f64, f64)> {
    let uuid = find(id)?.id;
    kwin::resize(&uuid, width, height)?;
    kwin::raise(&uuid)?;
    let w = kwin::window(&uuid)?.ok_or_else(|| anyhow!("window {id} closed while resizing"))?;
    Ok((w.rect[2], w.rect[3]))
}
