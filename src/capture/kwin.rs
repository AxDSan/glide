//! KWin (Plasma 6, Wayland) window and output queries.
//!
//! KWin exposes no window-list D-Bus API, so every query runs a short-lived KWin
//! script (loaded through `org.kde.kwin.Scripting`) that reports back by calling
//! `callDBus` on a one-shot service owned by this process. Geometry is logical px.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use zbus::blocking::{connection, Connection, Proxy};
use zbus::zvariant::OwnedValue;

const KWIN: &str = "org.kde.KWin";
const SCRIPTING: &str = "/Scripting";
const SCRIPTING_IFACE: &str = "org.kde.kwin.Scripting";
const SCRIPT_IFACE: &str = "org.kde.kwin.Script";
const WINDOW_QUERY: &str = "/KWin";
const INBOX_PATH: &str = "/org/glide/KWin";
const INBOX_IFACE: &str = "org.glide.KWin";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// `queryWindowInfo` blocks until the user clicks a window.
const PICK_TIMEOUT: Duration = Duration::from_secs(120);

/// A top-level window as KWin reports it. `rect` is the frame geometry (decorations included).
#[derive(Debug, Clone)]
pub struct KWindow {
    /// KWin internalId uuid, lowercase, without braces.
    pub id: String,
    /// KWin `resourceClass` (desktop-file style app id, e.g. `org.kde.dolphin`).
    pub app: String,
    pub title: String,
    pub pid: u32,
    /// x, y, w, h in logical px.
    pub rect: [f64; 4],
    pub output: String,
    pub active: bool,
    pub minimized: bool,
}

/// A connected, enabled screen. `rect` is in logical px; `scale` is the device pixel ratio.
#[derive(Debug, Clone)]
pub struct Output {
    pub name: String,
    pub rect: [f64; 4],
    pub scale: f64,
    pub primary: bool,
}

#[derive(Deserialize)]
struct Snapshot {
    windows: Vec<RawWindow>,
    screens: Vec<RawScreen>,
}

#[derive(Deserialize)]
struct RawWindow {
    id: String,
    app: String,
    title: String,
    pid: u32,
    rect: [f64; 4],
    output: String,
    active: bool,
    minimized: bool,
    normal: bool,
    desktop: bool,
    dock: bool,
    skip: bool,
}

#[derive(Deserialize)]
struct RawScreen {
    name: String,
    rect: [f64; 4],
    scale: f64,
}

#[derive(Deserialize)]
struct Active {
    active: bool,
}

#[derive(Deserialize)]
struct KscreenDoc {
    outputs: Vec<KscreenOutput>,
}

#[derive(Deserialize)]
struct KscreenOutput {
    name: String,
    #[serde(default)]
    enabled: bool,
    priority: i64,
}

/// Every listed (user-facing) top-level window, front to back.
pub fn windows() -> Result<Vec<KWindow>> {
    let snap = snapshot()?;
    let own = std::process::id();
    Ok(snap
        .windows
        .into_iter()
        .filter(|w| w.normal && !w.desktop && !w.dock && !w.skip && w.pid != own)
        .map(|w| KWindow {
            id: w.id,
            app: w.app,
            title: w.title,
            pid: w.pid,
            rect: w.rect,
            output: w.output,
            active: w.active,
            minimized: w.minimized,
        })
        .collect())
}

/// The listed window with this id, if any.
pub fn window(id: &str) -> Result<Option<KWindow>> {
    let id = normalize_id(id);
    Ok(windows()?.into_iter().find(|w| w.id == id))
}

/// The focused listed window, if any.
pub fn active() -> Result<Option<KWindow>> {
    Ok(windows()?.into_iter().find(|w| w.active))
}

/// Unminimizes, activates and raises the window. Errors if KWin does not activate it.
pub fn raise(id: &str) -> Result<()> {
    let id = normalize_id(id);
    let json = run_script(|svc| {
        script(
            svc,
            &format!("const ID = {};", js_str(&id)),
            "const w = find(ID);\n\
             if (!w) throw new Error(\"no such window\");\n\
             w.minimized = false;\n\
             workspace.activeWindow = w;\n\
             workspace.raiseWindow(w);\n\
             const a = workspace.activeWindow;\n\
             send({ active: !!a && uid(a) === ID });",
        )
    })?;
    let reply: Active = reply(&json)?;
    if !reply.active {
        bail!("KWin did not activate window {id}");
    }
    Ok(())
}

/// Like `raise`, but for this process's own topmost normal window, which `windows()`
/// excludes. Errors if KWin does not activate it.
pub fn raise_own() -> Result<()> {
    let own = std::process::id();
    let json = run_script(|svc| {
        script(
            svc,
            &format!("const OWN = {own};"),
            "let w = null;\n\
             for (const c of workspace.stackingOrder) if (!c.deleted && c.pid === OWN && c.normalWindow) w = c;\n\
             if (!w) throw new Error(\"no window for this process\");\n\
             const id = uid(w);\n\
             w.minimized = false;\n\
             workspace.activeWindow = w;\n\
             workspace.raiseWindow(w);\n\
             const a = workspace.activeWindow;\n\
             send({ active: !!a && uid(a) === id });",
        )
    })?;
    let reply: Active = reply(&json)?;
    if !reply.active {
        bail!("KWin did not activate the Glide window");
    }
    Ok(())
}

/// Sets the window's frame size to `w` x `h` logical px. The frame keeps its top-left corner
/// unless that would push it off its output; the size is capped at the output's size.
pub fn resize(id: &str, w: f64, h: f64) -> Result<()> {
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        bail!("invalid window size {w}x{h}");
    }
    let id = normalize_id(id);
    let json = run_script(|svc| {
        script(
            svc,
            &format!("const ID = {}; const W = {w}; const H = {h};", js_str(&id)),
            "const w = find(ID);\n\
             if (!w) throw new Error(\"no such window\");\n\
             if (!w.resizeable) throw new Error(\"window is not resizable\");\n\
             const o = w.output.geometry;\n\
             const f = w.frameGeometry;\n\
             const width = Math.min(W, o.width), height = Math.min(H, o.height);\n\
             const x = Math.max(o.x, Math.min(f.x, o.x + o.width - width));\n\
             const y = Math.max(o.y, Math.min(f.y, o.y + o.height - height));\n\
             w.frameGeometry = { x, y, width, height };\n\
             send({ rect: rect(w.frameGeometry) });",
        )
    })?;
    reply::<serde_json::Value>(&json)?;
    Ok(())
}

/// Waits for the user to click a window (KWin's interactive picker) and returns it if listed.
/// Returns `None` if the picker was cancelled or the clicked window is not listed.
/// Blocks until the click; call it off the UI thread.
pub fn pick() -> Result<Option<KWindow>> {
    let conn = connection::Builder::session()?
        .method_timeout(PICK_TIMEOUT)
        .build()?;
    let proxy = Proxy::new(&conn, KWIN, WINDOW_QUERY, "org.kde.KWin")?;
    let info: HashMap<String, OwnedValue> = proxy.call("queryWindowInfo", &())?;
    if info.is_empty() {
        return Ok(None);
    }
    let uuid = info
        .get("uuid")
        .context("queryWindowInfo reply has no uuid")?
        .try_clone()?;
    let uuid = String::try_from(uuid).context("queryWindowInfo uuid is not a string")?;
    window(&uuid)
}

/// Connected, enabled screens in logical px. Exactly one output is marked primary (KScreen
/// priority 1) when KScreen reports one.
pub fn outputs() -> Result<Vec<Output>> {
    let snap = snapshot()?;
    let primary = primary_output()?;
    Ok(snap
        .screens
        .into_iter()
        .map(|s| Output {
            primary: primary.as_deref() == Some(s.name.as_str()),
            name: s.name,
            rect: s.rect,
            scale: s.scale,
        })
        .collect())
}

fn snapshot() -> Result<Snapshot> {
    let json = run_script(|svc| script(svc, "", QUERY_BODY))?;
    reply(&json)
}

/// Parses a script reply. A bad shape reports the serde field error and the payload.
fn reply<T: DeserializeOwned>(json: &str) -> Result<T> {
    let v: serde_json::Value = serde_json::from_str(json)
        .with_context(|| format!("KWin reply is not JSON: {}", preview(json)))?;
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        bail!("KWin script failed: {e}");
    }
    serde_json::from_value(v)
        .with_context(|| format!("KWin reply has an unexpected shape: {}", preview(json)))
}

/// First 300 chars of a reply, for error messages.
fn preview(s: &str) -> String {
    s.chars().take(300).collect()
}

/// Name of the enabled output with KScreen priority 1.
fn primary_output() -> Result<Option<String>> {
    let out = Command::new("kscreen-doctor")
        .arg("-j")
        .output()
        .context("running kscreen-doctor -j")?;
    if !out.status.success() {
        bail!(
            "kscreen-doctor -j failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let doc: KscreenDoc = serde_json::from_slice(&out.stdout).context("parsing kscreen-doctor -j")?;
    Ok(doc
        .outputs
        .into_iter()
        .find(|o| o.enabled && o.priority == 1)
        .map(|o| o.name))
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Loads `make(service)` as a KWin script, runs it, and returns the JSON it delivers.
/// The script and its plugin are removed again on every path.
fn run_script(make: impl FnOnce(&str) -> String) -> Result<String> {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let service = format!("org.glide.KWin.p{pid}n{seq}");
    let plugin = format!("glide-kwin-p{pid}n{seq}");
    let path: PathBuf = std::env::temp_dir().join(format!("{plugin}.js"));

    let (tx, rx) = mpsc::channel::<String>();
    let conn = connection::Builder::session()?
        .method_timeout(CALL_TIMEOUT)
        .name(service.clone())?
        .serve_at(INBOX_PATH, Inbox(tx))?
        .build()?;

    std::fs::write(&path, make(&service)).context("writing KWin script")?;
    let _loaded = Loaded {
        conn: &conn,
        plugin: plugin.clone(),
        path: path.clone(),
    };

    let scripting = Proxy::new(&conn, KWIN, SCRIPTING, SCRIPTING_IFACE)?;
    let id: i32 = scripting.call(
        "loadScript",
        &(path.to_string_lossy().into_owned(), plugin.as_str()),
    )?;
    if id < 0 {
        bail!("KWin refused to load script {plugin}");
    }
    let script = Proxy::new(&conn, KWIN, format!("{SCRIPTING}/Script{id}"), SCRIPT_IFACE)?;
    script.call::<_, _, ()>("run", &())?;

    rx.recv_timeout(CALL_TIMEOUT)
        .context("KWin script did not report back (is KWin running with scripting enabled?)")
}

/// Unloads the script and deletes its file on drop.
struct Loaded<'a> {
    conn: &'a Connection,
    plugin: String,
    path: PathBuf,
}

impl Drop for Loaded<'_> {
    fn drop(&mut self) {
        // Cleanup only: a failed unload leaves nothing for the caller to act on.
        if let Ok(p) = Proxy::new(self.conn, KWIN, SCRIPTING, SCRIPTING_IFACE) {
            let _ = p.call::<_, _, bool>("unloadScript", &self.plugin);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Receives the script's reply on the `deliver` method.
struct Inbox(mpsc::Sender<String>);

#[zbus::interface(name = "org.glide.KWin")]
impl Inbox {
    #[zbus(name = "deliver")]
    fn deliver(&self, payload: String) {
        let _ = self.0.send(payload);
    }
}

/// KWin uuid form: lowercase, no braces.
fn normalize_id(id: &str) -> String {
    id.trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .to_ascii_lowercase()
}

/// JS string literal for `s`.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).expect("string serializes")
}

/// Script prelude: `send`, the id helpers, and the try/catch wrapper around `body`.
fn script(service: &str, args: &str, body: &str) -> String {
    format!(
        "const send = (o) => callDBus({svc}, {path}, {iface}, \"deliver\", JSON.stringify(o));\n\
         const uid = (w) => String(w.internalId).replace(/[{{}}]/g, \"\").toLowerCase();\n\
         const rect = (r) => [r.x, r.y, r.width, r.height];\n\
         const find = (id) => {{\n\
           for (const w of workspace.stackingOrder) if (!w.deleted && uid(w) === id) return w;\n\
           return null;\n\
         }};\n\
         {args}\n\
         try {{\n{body}\n}} catch (e) {{ send({{ error: String(e) }}); }}\n",
        svc = js_str(service),
        path = js_str(INBOX_PATH),
        iface = js_str(INBOX_IFACE),
    )
}

/// Reports every non-deleted window (bottom to top is reversed to front to back) and the screens.
const QUERY_BODY: &str = "\
const act = workspace.activeWindow;\n\
const activeId = act ? uid(act) : \"\";\n\
const finite = (r) => !!r && [r.x, r.y, r.width, r.height].every(Number.isFinite);\n\
const windows = [];\n\
for (const w of workspace.stackingOrder) {\n\
  if (w.deleted || !finite(w.frameGeometry)) continue;\n\
  const id = uid(w);\n\
  windows.push({ id, app: w.resourceClass || \"\", title: w.caption || \"\",\n\
    pid: Number.isInteger(w.pid) ? w.pid : 0, rect: rect(w.frameGeometry),\n\
    output: w.output && w.output.name ? w.output.name : \"\", active: id === activeId,\n\
    minimized: !!w.minimized, normal: !!w.normalWindow, desktop: !!w.desktopWindow,\n\
    dock: !!w.dock, skip: !!w.skipTaskbar });\n\
}\n\
windows.reverse();\n\
const screens = [];\n\
for (const s of workspace.screens) {\n\
  if (!finite(s.geometry) || !Number.isFinite(s.devicePixelRatio)) throw new Error(\"incomplete screen \" + s.name);\n\
  screens.push({ name: s.name || \"\", rect: rect(s.geometry), scale: s.devicePixelRatio });\n\
}\n\
send({ windows, screens });";
