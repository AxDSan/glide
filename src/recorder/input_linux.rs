//! Input logging for Linux: mouse buttons and keys from evdev (needs read access
//! to `/dev/input`, i.e. the `input` group). Pointer positions, cursor images and
//! the recorded bounds come from the portal stream, in stream pixels.

use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use evdev::{Device, EventType, InputEvent, KeyCode};

use crate::capture::screen::{CursorBitmap, CursorState};
use crate::capture::types::clock_secs;
use crate::engine::geom::Rect;
use crate::project::recording::{CursorImage, MouseKind, mods};

const TICK: Duration = Duration::from_micros(8_333);
/// Window bounds are sampled every this many ticks (about 100 ms).
const BOUNDS_EVERY: u64 = 12;
const NO_INPUT_DEVICES: &str = "permission denied reading /dev/input/event*: add your user to the `input` group (`sudo usermod -aG input $USER`), then log out and back in";

#[derive(Clone, Copy, Debug)]
pub struct RawMouse {
    pub host: f64,
    /// Stream pixels, top-left origin.
    pub x: f64,
    pub y: f64,
    pub kind: MouseKind,
    /// 0 left, 1 right, 2 middle.
    pub button: u8,
    /// `CursorImage.id` of the cursor shown at the time.
    pub cursor: u32,
}

#[derive(Clone, Debug)]
pub struct RawKey {
    pub host: f64,
    /// Carbon kVK code, as on macOS.
    pub code: u16,
    pub key: String,
    pub mods: u8,
    pub down: bool,
}

#[derive(Default)]
pub struct InputLog {
    pub mouse: Vec<RawMouse>,
    pub keys: Vec<RawKey>,
    pub cursors: Vec<CursorImage>,
    pub bounds: Vec<(f64, Rect)>,
    pub keys_enabled: bool,
}

/// Evdev key, Carbon kVK code, and US-QWERTY text (plain, shifted). Named keys have
/// empty text; their labels come from [`special`].
const KEYS: &[(KeyCode, u16, &str, &str)] = &[
    (KeyCode::KEY_A, 0x00, "a", "A"),
    (KeyCode::KEY_S, 0x01, "s", "S"),
    (KeyCode::KEY_D, 0x02, "d", "D"),
    (KeyCode::KEY_F, 0x03, "f", "F"),
    (KeyCode::KEY_H, 0x04, "h", "H"),
    (KeyCode::KEY_G, 0x05, "g", "G"),
    (KeyCode::KEY_Z, 0x06, "z", "Z"),
    (KeyCode::KEY_X, 0x07, "x", "X"),
    (KeyCode::KEY_C, 0x08, "c", "C"),
    (KeyCode::KEY_V, 0x09, "v", "V"),
    (KeyCode::KEY_B, 0x0B, "b", "B"),
    (KeyCode::KEY_Q, 0x0C, "q", "Q"),
    (KeyCode::KEY_W, 0x0D, "w", "W"),
    (KeyCode::KEY_E, 0x0E, "e", "E"),
    (KeyCode::KEY_R, 0x0F, "r", "R"),
    (KeyCode::KEY_Y, 0x10, "y", "Y"),
    (KeyCode::KEY_T, 0x11, "t", "T"),
    (KeyCode::KEY_1, 0x12, "1", "!"),
    (KeyCode::KEY_2, 0x13, "2", "@"),
    (KeyCode::KEY_3, 0x14, "3", "#"),
    (KeyCode::KEY_4, 0x15, "4", "$"),
    (KeyCode::KEY_6, 0x16, "6", "^"),
    (KeyCode::KEY_5, 0x17, "5", "%"),
    (KeyCode::KEY_EQUAL, 0x18, "=", "+"),
    (KeyCode::KEY_9, 0x19, "9", "("),
    (KeyCode::KEY_7, 0x1A, "7", "&"),
    (KeyCode::KEY_MINUS, 0x1B, "-", "_"),
    (KeyCode::KEY_8, 0x1C, "8", "*"),
    (KeyCode::KEY_0, 0x1D, "0", ")"),
    (KeyCode::KEY_RIGHTBRACE, 0x1E, "]", "}"),
    (KeyCode::KEY_O, 0x1F, "o", "O"),
    (KeyCode::KEY_U, 0x20, "u", "U"),
    (KeyCode::KEY_LEFTBRACE, 0x21, "[", "{"),
    (KeyCode::KEY_I, 0x22, "i", "I"),
    (KeyCode::KEY_P, 0x23, "p", "P"),
    (KeyCode::KEY_ENTER, 0x24, "", ""),
    (KeyCode::KEY_L, 0x25, "l", "L"),
    (KeyCode::KEY_J, 0x26, "j", "J"),
    (KeyCode::KEY_APOSTROPHE, 0x27, "'", "\""),
    (KeyCode::KEY_K, 0x28, "k", "K"),
    (KeyCode::KEY_SEMICOLON, 0x29, ";", ":"),
    (KeyCode::KEY_BACKSLASH, 0x2A, "\\", "|"),
    (KeyCode::KEY_COMMA, 0x2B, ",", "<"),
    (KeyCode::KEY_SLASH, 0x2C, "/", "?"),
    (KeyCode::KEY_N, 0x2D, "n", "N"),
    (KeyCode::KEY_M, 0x2E, "m", "M"),
    (KeyCode::KEY_DOT, 0x2F, ".", ">"),
    (KeyCode::KEY_TAB, 0x30, "", ""),
    (KeyCode::KEY_SPACE, 0x31, "", ""),
    (KeyCode::KEY_GRAVE, 0x32, "`", "~"),
    (KeyCode::KEY_BACKSPACE, 0x33, "", ""),
    (KeyCode::KEY_ESC, 0x35, "", ""),
    (KeyCode::KEY_DELETE, 0x75, "", ""),
    (KeyCode::KEY_HOME, 0x73, "", ""),
    (KeyCode::KEY_END, 0x77, "", ""),
    (KeyCode::KEY_PAGEUP, 0x74, "", ""),
    (KeyCode::KEY_PAGEDOWN, 0x79, "", ""),
    (KeyCode::KEY_LEFT, 0x7B, "", ""),
    (KeyCode::KEY_RIGHT, 0x7C, "", ""),
    (KeyCode::KEY_DOWN, 0x7D, "", ""),
    (KeyCode::KEY_UP, 0x7E, "", ""),
    (KeyCode::KEY_F1, 0x7A, "", ""),
    (KeyCode::KEY_F2, 0x78, "", ""),
    (KeyCode::KEY_F3, 0x63, "", ""),
    (KeyCode::KEY_F4, 0x76, "", ""),
    (KeyCode::KEY_F5, 0x60, "", ""),
    (KeyCode::KEY_F6, 0x61, "", ""),
    (KeyCode::KEY_F7, 0x62, "", ""),
    (KeyCode::KEY_F8, 0x64, "", ""),
    (KeyCode::KEY_F9, 0x65, "", ""),
    (KeyCode::KEY_F10, 0x6D, "", ""),
    (KeyCode::KEY_F11, 0x67, "", ""),
    (KeyCode::KEY_F12, 0x6F, "", ""),
    (KeyCode::KEY_F13, 0x69, "", ""),
    (KeyCode::KEY_F14, 0x6B, "", ""),
    (KeyCode::KEY_F15, 0x71, "", ""),
    (KeyCode::KEY_F16, 0x6A, "", ""),
    (KeyCode::KEY_F17, 0x40, "", ""),
    (KeyCode::KEY_F18, 0x4F, "", ""),
    (KeyCode::KEY_F19, 0x50, "", ""),
    (KeyCode::KEY_F20, 0x5A, "", ""),
    (KeyCode::KEY_KP0, 0x52, "0", "0"),
    (KeyCode::KEY_KP1, 0x53, "1", "1"),
    (KeyCode::KEY_KP2, 0x54, "2", "2"),
    (KeyCode::KEY_KP3, 0x55, "3", "3"),
    (KeyCode::KEY_KP4, 0x56, "4", "4"),
    (KeyCode::KEY_KP5, 0x57, "5", "5"),
    (KeyCode::KEY_KP6, 0x58, "6", "6"),
    (KeyCode::KEY_KP7, 0x59, "7", "7"),
    (KeyCode::KEY_KP8, 0x5B, "8", "8"),
    (KeyCode::KEY_KP9, 0x5C, "9", "9"),
    (KeyCode::KEY_KPDOT, 0x41, ".", "."),
    (KeyCode::KEY_KPASTERISK, 0x43, "*", "*"),
    (KeyCode::KEY_KPPLUS, 0x45, "+", "+"),
    (KeyCode::KEY_KPSLASH, 0x4B, "/", "/"),
    (KeyCode::KEY_KPENTER, 0x4C, "", ""),
    (KeyCode::KEY_KPMINUS, 0x4E, "-", "-"),
    (KeyCode::KEY_KPEQUAL, 0x51, "=", "="),
];

/// Labels for named keys, by kVK code (same glyphs as the macOS logger).
fn special(kvk: u16) -> Option<&'static str> {
    Some(match kvk {
        0x24 | 0x4C => "⏎",
        0x30 => "⇥",
        0x31 => "Space",
        0x33 => "⌫",
        0x35 => "⎋",
        0x75 => "⌦",
        0x7B => "←",
        0x7C => "→",
        0x7D => "↓",
        0x7E => "↑",
        0x73 => "Home",
        0x77 => "End",
        0x74 => "⇞",
        0x79 => "⇟",
        0x7A => "F1",
        0x78 => "F2",
        0x63 => "F3",
        0x76 => "F4",
        0x60 => "F5",
        0x61 => "F6",
        0x62 => "F7",
        0x64 => "F8",
        0x65 => "F9",
        0x6D => "F10",
        0x67 => "F11",
        0x6F => "F12",
        0x69 => "F13",
        0x6B => "F14",
        0x71 => "F15",
        0x6A => "F16",
        0x40 => "F17",
        0x4F => "F18",
        0x50 => "F19",
        0x5A => "F20",
        _ => return None,
    })
}

fn key_label(kvk: u16, text: &str, shifted: &str, shift: bool) -> String {
    match special(kvk) {
        Some(s) => s.to_string(),
        None => {
            let t = if shift { shifted } else { text };
            t.to_uppercase()
        }
    }
}

fn modifier_bit(code: KeyCode) -> Option<u8> {
    Some(match code {
        KeyCode::KEY_LEFTCTRL | KeyCode::KEY_RIGHTCTRL => mods::CTRL,
        KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT => mods::SHIFT,
        KeyCode::KEY_LEFTALT | KeyCode::KEY_RIGHTALT => mods::OPT,
        KeyCode::KEY_LEFTMETA | KeyCode::KEY_RIGHTMETA => mods::CMD,
        KeyCode::KEY_FN => mods::FN,
        _ => return None,
    })
}

struct Shared {
    stop: AtomicBool,
    /// Held mouse buttons, one bit per `RawMouse.button`.
    held: AtomicU8,
    /// Modifier state, `engine::keys` bit layout.
    mods: AtomicU8,
    /// `CursorImage.id` of the cursor shown now (0 = none yet).
    cursor: AtomicU32,
    /// Last visible pointer position, used for button events.
    last: Mutex<Option<(f64, f64)>>,
    log: Mutex<InputLog>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Running input logger. Workers stop on `stop` or drop.
pub struct InputLogger {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl InputLogger {
    /// Start logging. `cursor` samples the portal cursor each tick, `bounds` the
    /// recorded area; both return stream pixels and run on a worker thread. Pass
    /// `|| None` for `bounds` when there is no area to track. Keys are logged only
    /// when `log_keys` is set. Errors when `/dev/input` cannot be read.
    pub fn start(
        cursor_dir: PathBuf,
        cursor: impl Fn() -> Option<CursorState> + Send + 'static,
        bounds: impl Fn() -> Option<Rect> + Send + 'static,
        log_keys: bool,
    ) -> Result<Self> {
        std::fs::create_dir_all(&cursor_dir).with_context(|| format!("create {}", cursor_dir.display()))?;
        let devices = open_devices(log_keys)?;
        let keys_enabled = devices.iter().any(|d| d.keyboard);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            held: AtomicU8::new(0),
            mods: AtomicU8::new(0),
            cursor: AtomicU32::new(0),
            last: Mutex::new(None),
            log: Mutex::new(InputLog { keys_enabled, ..Default::default() }),
        });
        let mut threads = Vec::new();
        for d in devices {
            let sh = Arc::clone(&shared);
            threads.push(std::thread::spawn(move || read_device(sh, d.dev, d.name, d.pointer, d.keyboard)));
        }
        let sh = Arc::clone(&shared);
        threads.push(std::thread::spawn(move || poll(sh, cursor_dir, cursor, bounds)));
        Ok(Self { shared, threads })
    }

    /// Stop all workers and return the log, mouse and keys sorted by host time.
    pub fn stop(mut self) -> InputLog {
        self.shared.stop.store(true, Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let mut log = std::mem::take(&mut *lock(&self.shared.log));
        log.mouse.sort_by(|a, b| a.host.total_cmp(&b.host));
        log.keys.sort_by(|a, b| a.host.total_cmp(&b.host));
        log
    }
}

impl Drop for InputLogger {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
    }
}

struct Opened {
    dev: Device,
    name: String,
    pointer: bool,
    keyboard: bool,
}

/// Open every `/dev/input/event*` node and keep pointers, plus keyboards when
/// `log_keys` is set.
fn open_devices(log_keys: bool) -> Result<Vec<Opened>> {
    let mut out = Vec::new();
    let mut denied = 0usize;
    for entry in std::fs::read_dir("/dev/input").context("read /dev/input")? {
        let Ok(entry) = entry else { continue };
        if !entry.file_name().to_string_lossy().starts_with("event") {
            continue;
        }
        let dev = match Device::open(entry.path()) {
            Ok(d) => d,
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                denied += 1;
                continue;
            }
            // Node vanished, or is not readable for another reason.
            Err(_) => continue,
        };
        let pointer = is_pointer(&dev);
        let keyboard = log_keys && is_keyboard(&dev);
        if pointer || keyboard {
            let name = dev.name().unwrap_or("unnamed").to_string();
            out.push(Opened { dev, name, pointer, keyboard });
        }
    }
    if out.is_empty() {
        if denied > 0 {
            bail!(NO_INPUT_DEVICES);
        }
        bail!("no keyboard or mouse found under /dev/input");
    }
    if denied > 0 {
        eprintln!("glide: {denied} input device(s) unreadable; add your user to the `input` group for full input logging");
    }
    Ok(out)
}

/// Mouse-like: left button and a pointing axis pair (relative mice, or absolute
/// touchpads and tablets).
fn is_pointer(dev: &Device) -> bool {
    let button = dev.supported_keys().is_some_and(|k| k.contains(KeyCode::BTN_LEFT));
    let rel = dev
        .supported_relative_axes()
        .is_some_and(|a| a.contains(evdev::RelativeAxisCode::REL_X) && a.contains(evdev::RelativeAxisCode::REL_Y));
    let abs = dev
        .supported_absolute_axes()
        .is_some_and(|a| a.contains(evdev::AbsoluteAxisCode::ABS_X) && a.contains(evdev::AbsoluteAxisCode::ABS_Y));
    button && (rel || abs)
}

/// Keyboard-like: supports every ASCII letter key.
fn is_keyboard(dev: &Device) -> bool {
    dev.supported_keys().is_some_and(|k| {
        KEYS.iter()
            .filter(|e| e.2.len() == 1 && e.2.as_bytes()[0].is_ascii_alphabetic())
            .all(|e| k.contains(e.0))
    })
}

fn read_device(sh: Arc<Shared>, mut dev: Device, name: String, pointer: bool, keys: bool) {
    let fd = dev.as_raw_fd();
    while !sh.stop.load(Relaxed) {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd on the device fd owned by this thread.
        let n = unsafe { libc::poll(&mut pfd, 1, 50) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == ErrorKind::Interrupted {
                continue;
            }
            eprintln!("glide: input device {name} stopped: {e}");
            return;
        }
        if n == 0 {
            continue;
        }
        match dev.fetch_events() {
            Ok(events) => {
                for ev in events {
                    on_event(&sh, ev, pointer, keys);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => {
                eprintln!("glide: input device {name} stopped: {e}");
                return;
            }
        }
    }
}

fn on_event(sh: &Shared, ev: InputEvent, pointer: bool, keys: bool) {
    if ev.event_type() != EventType::KEY {
        return;
    }
    let code = KeyCode(ev.code());
    match code {
        KeyCode::BTN_LEFT | KeyCode::BTN_RIGHT | KeyCode::BTN_MIDDLE if pointer => button(sh, code, ev.value()),
        _ if keys => key(sh, code, ev.value()),
        _ => {}
    }
}

fn button(sh: &Shared, code: KeyCode, value: i32) {
    let bit: u8 = match code {
        KeyCode::BTN_LEFT => 0,
        KeyCode::BTN_RIGHT => 1,
        _ => 2,
    };
    let kind = match value {
        1 => {
            sh.held.fetch_or(1u8 << bit, Relaxed);
            MouseKind::Down
        }
        0 => {
            sh.held.fetch_and(!(1u8 << bit), Relaxed);
            MouseKind::Up
        }
        _ => return,
    };
    let Some((x, y)) = *lock(&sh.last) else { return };
    let host = clock_secs();
    lock(&sh.log).mouse.push(RawMouse { host, x, y, kind, button: bit, cursor: sh.cursor.load(Relaxed) });
}

fn key(sh: &Shared, code: KeyCode, value: i32) {
    if let Some(bit) = modifier_bit(code) {
        match value {
            1 => {
                sh.mods.fetch_or(bit, Relaxed);
            }
            0 => {
                sh.mods.fetch_and(!bit, Relaxed);
            }
            _ => {}
        }
        return;
    }
    // Press and autorepeat count as down, as on macOS.
    let down = match value {
        0 => false,
        1 | 2 => true,
        _ => return,
    };
    let Some(&(_, kvk, text, shifted)) = KEYS.iter().find(|k| k.0 == code) else { return };
    let m = sh.mods.load(Relaxed);
    let key = key_label(kvk, text, shifted, (m & mods::SHIFT) != 0);
    let host = clock_secs();
    lock(&sh.log).keys.push(RawKey { host, code: kvk, key, mods: m, down });
}

/// Samples the cursor each tick, logs moves and drags, records new cursor
/// images, and samples bounds.
fn poll(sh: Arc<Shared>, dir: PathBuf, cursor: impl Fn() -> Option<CursorState>, bounds: impl Fn() -> Option<Rect>) {
    let mut tick = 0u64;
    let mut moved: Option<(f64, f64)> = None;
    let mut seen_bitmap: Option<u64> = None;
    let mut last_bounds: Option<Rect> = None;
    while !sh.stop.load(Relaxed) {
        if let Some(c) = cursor() {
            if c.visible {
                let p = (c.x, c.y);
                *lock(&sh.last) = Some(p);
                if moved != Some(p) {
                    moved = Some(p);
                    let held = sh.held.load(Relaxed);
                    let (kind, button) = if held != 0 {
                        (MouseKind::Drag, held.trailing_zeros() as u8)
                    } else {
                        (MouseKind::Move, 0)
                    };
                    let host = clock_secs();
                    lock(&sh.log).mouse.push(RawMouse {
                        host,
                        x: p.0,
                        y: p.1,
                        kind,
                        button,
                        cursor: sh.cursor.load(Relaxed),
                    });
                }
            }
            if let Some(b) = c.bitmap {
                if seen_bitmap != Some(b.id) {
                    seen_bitmap = Some(b.id);
                    add_cursor(&sh, &dir, &b);
                }
            }
        }
        if tick % BOUNDS_EVERY == 0 {
            if let Some(r) = bounds() {
                if last_bounds != Some(r) {
                    last_bounds = Some(r);
                    let host = clock_secs();
                    lock(&sh.log).bounds.push((host, r));
                }
            }
        }
        tick += 1;
        std::thread::sleep(TICK);
    }
}

/// Save a new cursor bitmap as `cursor-{id}.png` and make it the current cursor.
fn add_cursor(sh: &Shared, dir: &Path, b: &CursorBitmap) {
    let (w, h) = (b.w as u32, b.h as u32);
    if w == 0 || h == 0 {
        return;
    }
    let id = lock(&sh.log).cursors.len() as u32 + 1;
    let file = format!("cursor-{id}.png");
    let saved = save_png(&dir.join(&file), w, h, &b.bgra);
    let size = (w as f64, h as f64);
    let hotspot = (b.hot.0 as f64 / size.0, b.hot.1 as f64 / size.1);
    let mut log = lock(&sh.log);
    log.cursors.push(CursorImage { id, file: saved.then_some(file), hotspot, size, system: None });
    sh.cursor.store(id, Relaxed);
}

fn save_png(path: &Path, w: u32, h: u32, bgra: &[u8]) -> bool {
    let rgba: Vec<u8> = bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    image::RgbaImage::from_raw(w, h, rgba).is_some_and(|img| img.save_with_format(path, image::ImageFormat::Png).is_ok())
}
