//! Desktop integration for Linux (KDE Plasma on Wayland): file reveal, open,
//! clipboard, trash and wallpaper, through the freedesktop interfaces and the
//! usual command-line helpers.

use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use parking_lot::Mutex;

/// Run a helper without waiting on it in the UI thread; a watcher thread reaps it.
fn spawn_detached(cmd: &mut Command) {
    let program = cmd.get_program().to_string_lossy().into_owned();
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("glide: couldn't run {program}: {e}"),
    }
}

/// `file://` URI for a path, percent-encoding everything outside the unreserved set.
fn file_uri(path: &Path) -> String {
    let mut s = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => s.push(b as char),
            _ => {
                let _ = write!(s, "%{b:02X}");
            }
        }
    }
    s
}

/// Bring the app to the front. Wayland doesn't let an app raise its own
/// window; winit's `focus_window` is used where the compositor allows it.
pub fn activate() {}

/// Reveal a file in the file manager, selected (org.freedesktop.FileManager1);
/// falls back to opening its folder when no file manager implements it.
pub fn reveal(path: &Path) {
    let uri = file_uri(path);
    let shown = zbus::blocking::Connection::session()
        .and_then(|c| {
            c.call_method(
                Some("org.freedesktop.FileManager1"),
                "/org/freedesktop/FileManager1",
                Some("org.freedesktop.FileManager1"),
                "ShowItems",
                &(vec![uri.as_str()], ""),
            )
        })
        .is_ok();
    if !shown {
        if let Some(dir) = path.parent() {
            open(dir);
        }
    }
}

/// Open a folder in the file manager.
pub fn open_folder(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    open(dir);
}

/// Open a file with its default application.
pub fn open(path: &Path) {
    spawn_detached(Command::new("xdg-open").arg(path));
}

/// Put a file on the clipboard as a `text/uri-list` (paste into Dolphin, Slack, Mail…).
/// `wl-copy` forks to keep serving the selection after this returns.
pub fn copy_file(path: &Path) {
    let mut child = match Command::new("wl-copy").args(["--type", "text/uri-list"]).stdin(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("glide: couldn't copy the file (wl-copy: {e})");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write as _;
        let _ = writeln!(stdin, "{}", file_uri(path));
    }
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Move a file or package to the trash (freedesktop trash spec).
pub fn trash(path: &Path) {
    if let Err(e) = trash::delete(path) {
        eprintln!("glide: couldn't trash {}: {e}", path.display());
    }
}

/// The wallpaper image of the desktop Plasma is showing, from its applet config.
fn plasma_wallpaper() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let cfg = PathBuf::from(home).join(".config/plasma-org.kde.plasma.desktop-appletsrc");
    let text = std::fs::read_to_string(cfg).ok()?;
    let mut section = "";
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('[') {
            if line.ends_with(']') && !rest.is_empty() {
                section = line;
            }
            continue;
        }
        if section.ends_with("[Wallpaper][org.kde.image][General]") {
            if let Some(value) = line.strip_prefix("Image=") {
                let path = PathBuf::from(value.strip_prefix("file://").unwrap_or(value));
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// The largest image in a Plasma wallpaper package (`contents/images`), which is
/// the full-resolution variant in the packages Plasma ships.
fn largest_image(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            matches!(p.extension().and_then(|e| e.to_str()), Some("jpg" | "jpeg" | "png" | "webp"))
        })
        .max_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
}

/// Save the current desktop wallpaper as a PNG at `dest`. Plasma wallpapers
/// are usually JPEG; formats the `image` crate can't decode are reported.
pub fn save_desktop_wallpaper(dest: &Path) -> Result<()> {
    let src = plasma_wallpaper().ok_or_else(|| anyhow!("Plasma doesn't report a desktop wallpaper image"))?;
    let file = if src.is_dir() {
        largest_image(&src.join("contents/images")).ok_or_else(|| anyhow!("no image in the wallpaper package {}", src.display()))?
    } else {
        src
    };
    let img = image::open(&file).with_context(|| format!("couldn't decode the desktop wallpaper {}", file.display()))?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    img.save_with_format(dest, image::ImageFormat::Png)?;
    Ok(())
}

/// The display list is cached briefly: `display_frame` asks for it on every UI tick, and each
/// query runs KWin scripts and a helper process on the UI thread.
static DISPLAYS: Mutex<Option<(Instant, Vec<crate::capture::screen::DisplayInfo>)>> = Mutex::new(None);
const DISPLAYS_TTL: Duration = Duration::from_secs(2);

/// The displays (KWin output ids), in KWin's order. Empty when KWin can't be reached.
fn displays() -> Vec<crate::capture::screen::DisplayInfo> {
    let mut cache = DISPLAYS.lock();
    if let Some((at, ds)) = cache.as_ref() {
        if at.elapsed() < DISPLAYS_TTL {
            return ds.clone();
        }
    }
    let ds = match crate::capture::screen::displays() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("glide: couldn't list displays: {e:#}");
            vec![]
        }
    };
    *cache = Some((Instant::now(), ds.clone()));
    ds
}

/// IDs of the active displays. Wayland has no global coordinates, so an id is
/// the KWin output id and overlays are placed by `monitor_for_display`.
pub fn display_ids() -> Vec<u32> {
    displays().iter().map(|d| d.id).collect()
}

/// The first display KWin reports (the one Plasma treats as primary).
pub fn main_display() -> u32 {
    displays().first().map_or(0, |d| d.id)
}

/// `(id, x, y, width, height)` in points, as `macos::display_frame`. Wayland clients can't
/// read global positions, so x and y are 0; overlays are placed by `monitor_for_display`.
pub fn display_frame(id: u32) -> (u32, f64, f64, f64, f64) {
    let ds = displays();
    match if id == 0 { ds.first() } else { ds.iter().find(|d| d.id == id) } {
        Some(d) => (d.id, 0.0, 0.0, d.points.0 as f64, d.points.1 as f64),
        None => (id, 0.0, 0.0, 0.0, 0.0),
    }
}

/// KDE draws the window decorations, so there are no traffic lights to inset.
pub fn inset_traffic_lights(_window: &winit::window::Window, _x: f64, _y: f64) {}

/// Single-instance socket shared by every Glide: a later `glide <project>` hands its path over.
fn open_socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("glide.sock")
}

/// Give `path` (absolute) to a running Glide. False when none is listening.
pub fn forward_open(path: &Path) -> bool {
    use std::io::Write;
    let Ok(mut s) = std::os::unix::net::UnixStream::connect(open_socket()) else { return false };
    if s.write_all(path.as_os_str().as_bytes()).is_err() || s.write_all(b"\n").is_err() {
        return false;
    }
    true
}

/// macOS delivers `.glide` opens through the app delegate; here a listener thread takes the
/// paths sent by later launches and opens them the same way.
pub fn install_open_handler() {
    use std::io::BufRead;
    let sock = open_socket();
    let _ = std::fs::remove_file(&sock);
    let listener = match std::os::unix::net::UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("glide: couldn't listen for opened projects at {}: {e}", sock.display());
            return;
        }
    };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            if std::io::BufReader::new(stream).read_line(&mut line).is_err() {
                continue;
            }
            let path = PathBuf::from(line.trim_end_matches('\n'));
            if path.extension().and_then(|e| e.to_str()) == Some(crate::project::EXTENSION) {
                super::push(super::Cmd::Open(path));
            }
        }
    });
}

/// Wayland exposes no global pointer position to clients, so this is the main display.
pub fn display_under_pointer() -> u32 {
    main_display()
}

/// Whether the window is fullscreen (winit reports it for any compositor).
pub fn is_fullscreen(window: &winit::window::Window) -> bool {
    window.fullscreen().is_some()
}

/// The display a fullscreen window is on.
pub fn fullscreen_display(window: &winit::window::Window) -> Option<u32> {
    if !is_fullscreen(window) {
        return None;
    }
    display_of_monitor(&window.current_monitor()?)
}

/// The KWin display matching a winit monitor. Both report the physical size of
/// the same output, which identifies it.
fn display_of_monitor(m: &winit::monitor::MonitorHandle) -> Option<u32> {
    let s = m.size();
    let px = (s.width as usize, s.height as usize);
    displays().iter().find(|d| d.pixels == px).map(|d| d.id)
}

/// The winit monitor for a display id (for `Fullscreen::Borderless`).
pub fn monitor_for_display(el: &winit::event_loop::ActiveEventLoop, display: u32) -> Option<winit::monitor::MonitorHandle> {
    let want = displays().into_iter().find(|d| d.id == display)?.pixels;
    el.available_monitors().find(|m| {
        let s = m.size();
        (s.width as usize, s.height as usize) == want
    })
}
