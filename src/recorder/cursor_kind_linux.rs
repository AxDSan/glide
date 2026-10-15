//! Names recorded cursor images ("arrow", "pointing-hand" …) on Linux by
//! comparing their shape with the user's Xcursor theme. The compositor sends
//! only bitmaps, and themes styled differently (or rendered from SVG at another
//! size) rarely match pixel for pixel, so shapes are compared as normalised
//! coverage grids plus the hotspot's place in the shape.

use std::path::{Path, PathBuf};

/// Cursor names (as on macOS) and the Xcursor file names that draw them.
const KINDS: &[(&str, &[&str])] = &[
    ("arrow", &["default", "left_ptr", "arrow"]),
    ("pointing-hand", &["pointer", "hand2", "pointing_hand", "hand1"]),
    ("ibeam", &["text", "xterm", "ibeam"]),
    ("open-hand", &["openhand", "grab"]),
    ("closed-hand", &["closedhand", "grabbing"]),
    ("crosshair", &["crosshair", "cross"]),
    ("resize-left-right", &["ew-resize", "size_hor", "col-resize", "h_double_arrow"]),
    ("resize-up-down", &["ns-resize", "size_ver", "row-resize", "v_double_arrow"]),
];

const GRID: usize = 16;
/// Largest distance still accepted as a match. On a 54 px MacTahoe recording the
/// right kind scored 0.12 to 0.18 and the nearest wrong one 0.53 or more.
const MAX_DISTANCE: f64 = 0.35;

/// Shape summary of one cursor image.
struct Shape {
    grid: [f32; GRID * GRID],
    /// Hotspot relative to the opaque bounding box.
    hot: (f64, f64),
    aspect: f64,
}

/// Reference shapes from the user's cursor theme.
pub struct Classifier {
    refs: Vec<(&'static str, Shape)>,
}

impl Classifier {
    /// Loads the active cursor theme, picking the size nearest `size_px`.
    /// None when no theme with any of the known cursors is found.
    pub fn load(size_px: u32) -> Option<Self> {
        let theme = theme_name();
        let dirs = icon_dirs();
        let mut refs = Vec::new();
        for (kind, names) in KINDS {
            let found = names.iter().find_map(|n| find_cursor(&dirs, &theme, n, 0));
            if let Some(shape) = found.and_then(|p| std::fs::read(p).ok()).and_then(|b| parse_xcursor(&b, size_px)) {
                refs.push((*kind, shape));
            }
        }
        (!refs.is_empty()).then_some(Classifier { refs })
    }

    /// The cursor name for a BGRA image with its hotspot in pixels, if one matches.
    pub fn classify(&self, w: usize, h: usize, bgra: &[u8], hot: (i32, i32)) -> Option<&'static str> {
        let s = shape(w, h, |x, y| bgra[(y * w + x) * 4 + 3], hot)?;
        self.refs
            .iter()
            .map(|(k, r)| (*k, distance(&s, r)))
            .filter(|(_, d)| *d <= MAX_DISTANCE)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(k, _)| k)
    }
}

fn distance(a: &Shape, b: &Shape) -> f64 {
    let grid = a.grid.iter().zip(&b.grid).map(|(x, y)| (x - y).abs() as f64).sum::<f64>() / (GRID * GRID) as f64;
    let hot = (a.hot.0 - b.hot.0).hypot(a.hot.1 - b.hot.1);
    let aspect = (a.aspect / b.aspect).ln().abs();
    grid + 0.5 * hot + 0.3 * aspect
}

/// Coverage grid of the opaque bounding box, or None for an empty image.
fn shape(w: usize, h: usize, alpha: impl Fn(usize, usize) -> u8, hot: (i32, i32)) -> Option<Shape> {
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for y in 0..h {
        for x in 0..w {
            if alpha(x, y) >= 128 {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
            }
        }
    }
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    let (bw, bh) = ((x1 - x0) as f64, (y1 - y0) as f64);
    let mut grid = [0f32; GRID * GRID];
    let mut count = [0u32; GRID * GRID];
    for y in y0..y1 {
        for x in x0..x1 {
            let gx = (((x - x0) as f64 / bw) * GRID as f64) as usize;
            let gy = (((y - y0) as f64 / bh) * GRID as f64) as usize;
            let i = gy.min(GRID - 1) * GRID + gx.min(GRID - 1);
            grid[i] += (alpha(x, y) >= 128) as u8 as f32;
            count[i] += 1;
        }
    }
    for (g, c) in grid.iter_mut().zip(count) {
        *g /= c.max(1) as f32;
    }
    let hot = ((hot.0 as f64 - x0 as f64) / bw, (hot.1 as f64 - y0 as f64) / bh);
    Some(Shape { grid, hot, aspect: bw / bh })
}

/// Reads the image nearest `size_px` from an Xcursor file.
fn parse_xcursor(b: &[u8], size_px: u32) -> Option<Shape> {
    const IMAGE: u32 = 0xfffd_0002;
    let u32_at = |o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()));
    if b.get(..4)? != b"Xcur" {
        return None;
    }
    let ntoc = u32_at(12)? as usize;
    let mut best: Option<(u32, usize)> = None;
    for i in 0..ntoc.min(1024) {
        let e = 16 + 12 * i;
        if u32_at(e)? != IMAGE {
            continue;
        }
        let pos = u32_at(e + 8)? as usize;
        let width = u32_at(pos + 16)?;
        if best.is_none_or(|(w, _)| width.abs_diff(size_px) < w.abs_diff(size_px)) {
            best = Some((width, pos));
        }
    }
    let pos = best?.1;
    let (w, h) = (u32_at(pos + 16)? as usize, u32_at(pos + 20)? as usize);
    let hot = (u32_at(pos + 24)? as i32, u32_at(pos + 28)? as i32);
    let px = b.get(pos + 36..pos + 36 + w * h * 4)?;
    // Pixels are little-endian ARGB words, so alpha is the fourth byte.
    shape(w, h, |x, y| px[(y * w + x) * 4 + 3], hot)
}

/// The configured cursor theme: Plasma's setting, then XCURSOR_THEME, then "default".
fn theme_name() -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    if let Ok(text) = std::fs::read_to_string(config.join("kcminputrc")) {
        let mut in_mouse = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_mouse = line == "[Mouse]";
            } else if in_mouse && let Some(v) = line.strip_prefix("cursorTheme=") {
                return v.trim().to_string();
            }
        }
    }
    std::env::var("XCURSOR_THEME").ok().filter(|t| !t.is_empty()).unwrap_or_else(|| "default".into())
}

/// Icon theme roots in lookup order.
fn icon_dirs() -> Vec<PathBuf> {
    if let Ok(p) = std::env::var("XCURSOR_PATH") {
        return std::env::split_paths(&p).collect();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let mut dirs = vec![data_home.join("icons"), home.join(".icons")];
    dirs.extend(data_dirs.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join("icons")));
    dirs.push("/usr/share/pixmaps".into());
    dirs
}

/// Finds `name` in `theme` or the themes it inherits from.
fn find_cursor(dirs: &[PathBuf], theme: &str, name: &str, depth: u32) -> Option<PathBuf> {
    if depth > 4 {
        return None;
    }
    for d in dirs {
        let p = d.join(theme).join("cursors").join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    for d in dirs {
        let Ok(index) = std::fs::read_to_string(d.join(theme).join("index.theme")) else { continue };
        let inherits = index.lines().find_map(|l| l.trim().strip_prefix("Inherits=")).unwrap_or("");
        for parent in inherits.split(',').map(str::trim).filter(|p| !p.is_empty() && *p != theme) {
            if let Some(p) = find_cursor(dirs, parent, name, depth + 1) {
                return Some(p);
            }
        }
    }
    None
}
