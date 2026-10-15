//! Global hotkeys on Linux via evdev. Every keyboard under `/dev/input` is read
//! directly, so combos fire regardless of focus and without portal or compositor
//! support. Keys are observed, not grabbed: the focused app still gets them.
//! Needs read access to `/dev/input/event*` (the `input` group). Callbacks run on
//! the reader thread of the keyboard that fired, so they must be `Send` and should
//! only queue work (`crate::app::push` does).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, Once, PoisonError};
use std::thread;
use std::time::Duration;

use evdev::{Device, EventType, KeyCode};
use serde::{Deserialize, Serialize};

pub const KEY_R: u32 = 15; // kVK_ANSI_R
pub const CMD: u32 = 1 << 8; // Super/Meta
pub const SHIFT: u32 = 1 << 9;
pub const OPTION: u32 = 1 << 11; // Alt
pub const CONTROL: u32 = 1 << 12;
/// Modifiers for the default record/pause shortcuts (Ctrl+Alt+Shift). Super is
/// avoided because egui-winit does not forward it on Linux, so it could not be re-recorded.
pub const DEFAULT_MODS: u32 = CONTROL | OPTION | SHIFT;

/// macOS virtual key code, evdev key, and egui key name (used for display and `from_egui`).
const KEYS: &[(u32, KeyCode, &str)] = &[
    (0, KeyCode::KEY_A, "A"),
    (11, KeyCode::KEY_B, "B"),
    (8, KeyCode::KEY_C, "C"),
    (2, KeyCode::KEY_D, "D"),
    (14, KeyCode::KEY_E, "E"),
    (3, KeyCode::KEY_F, "F"),
    (5, KeyCode::KEY_G, "G"),
    (4, KeyCode::KEY_H, "H"),
    (34, KeyCode::KEY_I, "I"),
    (38, KeyCode::KEY_J, "J"),
    (40, KeyCode::KEY_K, "K"),
    (37, KeyCode::KEY_L, "L"),
    (46, KeyCode::KEY_M, "M"),
    (45, KeyCode::KEY_N, "N"),
    (31, KeyCode::KEY_O, "O"),
    (35, KeyCode::KEY_P, "P"),
    (12, KeyCode::KEY_Q, "Q"),
    (15, KeyCode::KEY_R, "R"),
    (1, KeyCode::KEY_S, "S"),
    (17, KeyCode::KEY_T, "T"),
    (32, KeyCode::KEY_U, "U"),
    (9, KeyCode::KEY_V, "V"),
    (13, KeyCode::KEY_W, "W"),
    (7, KeyCode::KEY_X, "X"),
    (16, KeyCode::KEY_Y, "Y"),
    (6, KeyCode::KEY_Z, "Z"),
    (29, KeyCode::KEY_0, "0"),
    (18, KeyCode::KEY_1, "1"),
    (19, KeyCode::KEY_2, "2"),
    (20, KeyCode::KEY_3, "3"),
    (21, KeyCode::KEY_4, "4"),
    (23, KeyCode::KEY_5, "5"),
    (22, KeyCode::KEY_6, "6"),
    (26, KeyCode::KEY_7, "7"),
    (28, KeyCode::KEY_8, "8"),
    (25, KeyCode::KEY_9, "9"),
    (122, KeyCode::KEY_F1, "F1"),
    (120, KeyCode::KEY_F2, "F2"),
    (99, KeyCode::KEY_F3, "F3"),
    (118, KeyCode::KEY_F4, "F4"),
    (96, KeyCode::KEY_F5, "F5"),
    (97, KeyCode::KEY_F6, "F6"),
    (98, KeyCode::KEY_F7, "F7"),
    (100, KeyCode::KEY_F8, "F8"),
    (101, KeyCode::KEY_F9, "F9"),
    (109, KeyCode::KEY_F10, "F10"),
    (103, KeyCode::KEY_F11, "F11"),
    (111, KeyCode::KEY_F12, "F12"),
    (49, KeyCode::KEY_SPACE, "Space"),
    (36, KeyCode::KEY_ENTER, "Enter"),
    (53, KeyCode::KEY_ESC, "Escape"),
    (48, KeyCode::KEY_TAB, "Tab"),
    (51, KeyCode::KEY_BACKSPACE, "Backspace"),
    (117, KeyCode::KEY_DELETE, "Delete"),
    (115, KeyCode::KEY_HOME, "Home"),
    (119, KeyCode::KEY_END, "End"),
    (116, KeyCode::KEY_PAGEUP, "PageUp"),
    (121, KeyCode::KEY_PAGEDOWN, "PageDown"),
    (123, KeyCode::KEY_LEFT, "Left"),
    (124, KeyCode::KEY_RIGHT, "Right"),
    (125, KeyCode::KEY_DOWN, "Down"),
    (126, KeyCode::KEY_UP, "Up"),
];

/// A key combination: macOS virtual key code + Carbon-style modifier flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Combo {
    pub key: u32,
    pub mods: u32,
}

impl Combo {
    pub const fn new(key: u32, mods: u32) -> Self {
        Combo { key, mods }
    }

    /// "Super+Ctrl+Alt+Shift+R" style label (Qt order, as KDE writes shortcuts).
    pub fn label(&self) -> String {
        let mut s = String::new();
        for (bit, name) in [(CMD, "Super"), (CONTROL, "Ctrl"), (OPTION, "Alt"), (SHIFT, "Shift")] {
            if self.mods & bit != 0 {
                s.push_str(name);
                s.push('+');
            }
        }
        s.push_str(&key_name(self.key));
        s
    }

    /// From an egui key press; None for keys we can't map or combos without
    /// Ctrl or Alt (plain keys would hijack typing), unless the key is an F-key.
    pub fn from_egui(key: egui::Key, m: egui::Modifiers) -> Option<Combo> {
        use egui::Key::*;
        let code = egui_to_vk(key)?;
        let mut mods = 0;
        if m.ctrl {
            mods |= CONTROL;
        }
        if m.alt {
            mods |= OPTION;
        }
        if m.shift {
            mods |= SHIFT;
        }
        let is_fkey = matches!(key, F1 | F2 | F3 | F4 | F5 | F6 | F7 | F8 | F9 | F10 | F11 | F12);
        (mods & (CONTROL | OPTION) != 0 || is_fkey).then_some(Combo { key: code, mods })
    }
}

struct Binding {
    id: u32,
    combo: Combo,
    callback: Box<dyn Fn() + Send>,
}

static BINDINGS: Mutex<Vec<Binding>> = Mutex::new(Vec::new());
/// Keyboard device nodes that currently have a reader thread.
static KEYBOARDS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static START: Once = Once::new();

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Bind `combo` to slot `id`, replacing any previous binding of that slot.
/// Callbacks run on a keyboard reader thread. Returns:
/// - `Err(-1)`: another slot already has `combo`.
/// - `Err(-2)`: no keyboard could be opened (permissions; reason is logged).
/// - `Err(-3)`: `combo.key` has no evdev mapping.
pub fn bind(id: u32, combo: Combo, callback: impl Fn() + Send + 'static) -> Result<(), i32> {
    if !KEYS.iter().any(|(k, _, _)| *k == combo.key) {
        return Err(-3);
    }
    start();
    let have_keyboard = !lock(&KEYBOARDS).is_empty();
    let mut bindings = lock(&BINDINGS);
    if bindings.iter().any(|b| b.id != id && b.combo == combo) {
        return Err(-1);
    }
    if !have_keyboard {
        return Err(-2);
    }
    bindings.retain(|b| b.id != id);
    bindings.push(Binding { id, combo, callback: Box::new(callback) });
    Ok(())
}

/// Remove the binding of slot `id`, if any.
pub fn unbind(id: u32) {
    lock(&BINDINGS).retain(|b| b.id != id);
}

/// Single-binding convenience used before settings existed.
pub fn register(key: u32, modifiers: u32, callback: impl Fn() + Send + 'static) -> Result<(), i32> {
    bind(0, Combo::new(key, modifiers), callback)
}

/// Opens existing keyboards and starts the hotplug rescan thread, once.
fn start() {
    START.call_once(|| {
        let mut scanner = Scanner::default();
        scanner.scan();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(2));
            scanner.scan();
        });
    });
}

/// Tracks `/dev/input` nodes that were examined and must not be opened again.
#[derive(Default)]
struct Scanner {
    ignored: HashSet<PathBuf>,
}

impl Scanner {
    /// Opens each new `event*` node: keyboards get a reader thread; other
    /// devices, and nodes that fail to open, are remembered and skipped.
    fn scan(&mut self) {
        let Ok(dir) = std::fs::read_dir("/dev/input") else { return };
        let mut present = HashSet::new();
        for entry in dir.flatten() {
            let path = entry.path();
            if !path.file_name().is_some_and(|n| n.as_encoded_bytes().starts_with(b"event")) {
                continue;
            }
            present.insert(path.clone());
            if self.ignored.contains(&path) || lock(&KEYBOARDS).contains(&path) {
                continue;
            }
            match Device::open(&path) {
                Ok(dev) if is_keyboard(&dev) => spawn_reader(path, dev),
                Ok(_) => {
                    self.ignored.insert(path);
                }
                Err(e) => {
                    eprintln!("glide: can't read {}: {e} (is your user in the `input` group?)", path.display());
                    self.ignored.insert(path);
                }
            }
        }
        self.ignored.retain(|p| present.contains(p));
    }
}

fn is_keyboard(dev: &Device) -> bool {
    dev.supported_keys().is_some_and(|k| k.contains(KeyCode::KEY_A) && k.contains(KeyCode::KEY_ENTER))
}

fn spawn_reader(path: PathBuf, dev: Device) {
    lock(&KEYBOARDS).push(path.clone());
    thread::spawn(move || read_keyboard(path, dev));
}

/// Reads one keyboard until it disconnects, tracking its modifier state.
fn read_keyboard(path: PathBuf, mut dev: Device) {
    let mut held = 0;
    while let Ok(events) = dev.fetch_events() {
        for ev in events {
            if ev.event_type() != EventType::KEY {
                continue;
            }
            let code = KeyCode::new(ev.code());
            let value = ev.value();
            if let Some(bit) = modifier_bit(code) {
                match value {
                    1 => held |= bit,
                    0 => held &= !bit,
                    _ => {}
                }
                continue;
            }
            // value 1 = press; 2 = autorepeat, ignored.
            if value == 1 {
                if let Some(vk) = evdev_to_vk(code) {
                    fire(vk, held);
                }
            }
        }
    }
    lock(&KEYBOARDS).retain(|p| *p != path);
}

fn fire(vk: u32, mods: u32) {
    let bindings = lock(&BINDINGS);
    for b in bindings.iter().filter(|b| b.combo.key == vk && b.combo.mods == mods) {
        (b.callback)();
    }
}

fn modifier_bit(code: KeyCode) -> Option<u32> {
    match code {
        KeyCode::KEY_LEFTCTRL | KeyCode::KEY_RIGHTCTRL => Some(CONTROL),
        KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT => Some(SHIFT),
        KeyCode::KEY_LEFTALT | KeyCode::KEY_RIGHTALT => Some(OPTION),
        KeyCode::KEY_LEFTMETA | KeyCode::KEY_RIGHTMETA => Some(CMD),
        _ => None,
    }
}

fn evdev_to_vk(code: KeyCode) -> Option<u32> {
    KEYS.iter().find(|(_, c, _)| *c == code).map(|(k, _, _)| *k)
}

fn key_name(vk: u32) -> String {
    KEYS.iter()
        .find(|(k, _, _)| *k == vk)
        .map(|(_, _, n)| n.to_string())
        .unwrap_or_else(|| format!("key {vk}"))
}

fn egui_to_vk(key: egui::Key) -> Option<u32> {
    let name = key.name();
    KEYS.iter().find(|(_, _, n)| *n == name).map(|(k, _, _)| *k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_egui_mapping() {
        assert_eq!(Combo::new(KEY_R, DEFAULT_MODS).label(), "Ctrl+Alt+Shift+R");
        let m = egui::Modifiers { alt: true, shift: true, ctrl: true, ..Default::default() };
        assert_eq!(Combo::from_egui(egui::Key::R, m), Some(Combo::new(KEY_R, DEFAULT_MODS)));
        // Plain letters are rejected; F-keys alone are fine.
        assert_eq!(Combo::from_egui(egui::Key::R, egui::Modifiers::default()), None);
        assert_eq!(Combo::from_egui(egui::Key::F5, egui::Modifiers::default()), Some(Combo::new(96, 0)));
        assert_eq!(Combo::new(23, CONTROL).label(), "Ctrl+5");
    }
}
