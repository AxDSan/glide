//! Status notifier (ksni) tray item, the Linux counterpart of the mac menu bar item: a state icon
//! (grey idle, red recording, amber paused), the elapsed time as the title, and the same menu.
//! Menu actions are queued as `Cmd`s for the app.

use std::sync::atomic::{AtomicBool, Ordering};

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Icon, OfflineReason, ToolTip, Tray};

use super::Cmd;
use super::home::SourceKind;

const IDLE: (u8, u8, u8) = (0x9a, 0x9a, 0x9a);
const RECORDING: (u8, u8, u8) = (0xe5, 0x39, 0x35);
const PAUSED: (u8, u8, u8) = (0xf5, 0xa6, 0x23);

/// State shared with the tray service thread. ksni owns it; `MenuBar` updates it through `Handle`.
struct GlideTray {
    recording: bool,
    paused: bool,
    finishing: bool,
    /// Text the bar shows (elapsed time or "Saving…"), as on mac.
    text: String,
    /// Disabled menu line describing the global hotkey.
    hotkey: String,
    offline_logged: AtomicBool,
}

/// A standard item that queues `cmd` when activated.
fn command(label: &str, cmd: fn() -> Cmd) -> StandardItem<GlideTray> {
    StandardItem {
        label: label.into(),
        activate: Box::new(move |_: &mut GlideTray| super::push(cmd())),
        ..Default::default()
    }
}

/// Shortcut in SNI form: one key press made of modifiers followed by the key.
fn sc(keys: &[&str]) -> Vec<Vec<String>> {
    vec![keys.iter().map(|k| (*k).to_string()).collect()]
}

/// Square ARGB32 disc (network byte order, non-premultiplied), with two white pause bars if `pause`.
fn disc(size: i32, (r, g, b): (u8, u8, u8), pause: bool) -> Icon {
    let s = size as f32;
    let radius = s / 2.0 - 1.0;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (px, py) = ((x as f32 + 0.5) / s, (y as f32 + 0.5) / s);
            let dist = ((px - 0.5).powi(2) + (py - 0.5).powi(2)).sqrt() * s;
            let alpha = (radius + 0.5 - dist).clamp(0.0, 1.0);
            let in_bar = (0.34..=0.46).contains(&px) || (0.54..=0.66).contains(&px);
            let bar = pause && in_bar && (0.30..=0.70).contains(&py);
            let (cr, cg, cb) = if bar { (255, 255, 255) } else { (r, g, b) };
            data.extend_from_slice(&[(alpha * 255.0).round() as u8, cr, cg, cb]);
        }
    }
    Icon { width: size, height: size, data }
}

impl Tray for GlideTray {
    /// Left click opens the menu, as the mac status item does.
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "dev.glide.Glide".into()
    }

    fn title(&self) -> String {
        let t = self.text.trim();
        if t.is_empty() { "Glide".into() } else { t.into() }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let (rgb, pause) = if self.finishing {
            (IDLE, false)
        } else if self.paused {
            (PAUSED, true)
        } else if self.recording {
            (RECORDING, false)
        } else {
            (IDLE, false)
        };
        [22, 44].into_iter().map(|s| disc(s, rgb, pause)).collect()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip { title: self.title(), ..Default::default() }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let record = if self.finishing {
            "Saving…"
        } else if self.recording {
            "Stop Recording"
        } else {
            "Start Recording"
        };
        let pause = if self.paused { "Resume" } else { "Pause" };
        let sources = !(self.recording || self.finishing);
        vec![
            StandardItem {
                enabled: !self.finishing,
                shortcut: sc(&["Control", "Alt", "Shift", "R"]),
                ..command(record, || Cmd::ToggleRecording)
            }
            .into(),
            StandardItem {
                visible: sources,
                ..command("Record Display", || Cmd::RecordSource(SourceKind::Display))
            }
            .into(),
            StandardItem {
                visible: sources,
                ..command("Record Window…", || Cmd::RecordSource(SourceKind::Window))
            }
            .into(),
            StandardItem {
                visible: sources,
                ..command("Record Area…", || Cmd::RecordSource(SourceKind::Area))
            }
            .into(),
            StandardItem { enabled: self.recording, ..command(pause, || Cmd::TogglePause) }.into(),
            MenuItem::Separator,
            command("Show Glide", || Cmd::ShowWindow).into(),
            command("Open Projects Folder", || Cmd::OpenFolder).into(),
            MenuItem::Separator,
            StandardItem { label: self.hotkey.clone(), enabled: false, ..Default::default() }.into(),
            StandardItem { shortcut: sc(&["Control", "Q"]), ..command("Quit Glide", || Cmd::Quit) }.into(),
        ]
    }

    /// Keep waiting for the watcher (ksni re-registers when it returns); log the first error only.
    fn watcher_offline(&self, reason: OfflineReason) -> bool {
        if let OfflineReason::Error(e) = reason {
            if !self.offline_logged.swap(true, Ordering::Relaxed) {
                eprintln!("glide: tray icon waiting for a StatusNotifierWatcher: {e}");
            }
        }
        true
    }
}

pub struct MenuBar {
    /// `None` when the tray could not be created (no session bus); the app runs without it.
    tray: Option<Handle<GlideTray>>,
    last: Option<(bool, bool, bool, String)>,
}

/// What the menu bar shows.
pub struct View<'a> {
    pub recording: bool,
    pub paused: bool,
    pub finishing: bool,
    pub clock: &'a str,
}

/// Bar text for a state, identical to the mac status item title.
fn bar_text(recording: bool, paused: bool, finishing: bool, clock: &str) -> String {
    if finishing {
        " Saving…".into()
    } else if recording || paused {
        format!(" {clock}")
    } else {
        String::new()
    }
}

impl MenuBar {
    pub fn new() -> MenuBar {
        let tray = GlideTray {
            recording: false,
            paused: false,
            finishing: false,
            text: String::new(),
            hotkey: "No global shortcut".into(),
            offline_logged: AtomicBool::new(false),
        };
        // `assume_sni_available` makes a missing watcher non-fatal: the service keeps running and
        // registers once `org.kde.StatusNotifierWatcher` appears.
        let tray = match tray.assume_sni_available(true).spawn() {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("glide: tray icon unavailable: {e}");
                None
            }
        };
        let mut mb = MenuBar { tray, last: None };
        mb.update(&View { recording: false, paused: false, finishing: false, clock: "" });
        mb
    }

    pub fn set_hotkey(&self, label: &str, ok: bool) {
        let text = match (label.is_empty(), ok) {
            (true, _) => "No global shortcut".to_string(),
            (false, true) => format!("Global shortcut: {label}"),
            (false, false) => format!("Global shortcut {label} unavailable"),
        };
        if let Some(t) = &self.tray {
            t.update(move |g: &mut GlideTray| g.hotkey = text);
        }
    }

    pub fn title(&self) -> Option<String> {
        self.last.as_ref().map(|(r, p, f, c)| bar_text(*r, *p, *f, c))
    }

    pub fn update(&mut self, v: &View) {
        let key = (v.recording, v.paused, v.finishing, v.clock.to_string());
        if self.last.as_ref() == Some(&key) {
            return;
        }
        self.last = Some(key);
        let text = bar_text(v.recording, v.paused, v.finishing, v.clock);
        let (recording, paused, finishing) = (v.recording, v.paused, v.finishing);
        if let Some(t) = &self.tray {
            t.update(move |g: &mut GlideTray| {
                g.recording = recording;
                g.paused = paused;
                g.finishing = finishing;
                g.text = text;
            });
        }
    }
}

impl Drop for MenuBar {
    fn drop(&mut self) {
        if let Some(t) = self.tray.take() {
            t.shutdown().wait();
        }
    }
}
