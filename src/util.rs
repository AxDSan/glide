//! Small platform helpers: Objective-C / CoreFoundation bridging (macOS only),
//! app directories, and neutral utilities.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2::runtime::AnyObject;
#[cfg(target_os = "macos")]
use objc2_core_foundation::CFString;
#[cfg(target_os = "macos")]
use objc2_core_media::{CMClock, CMTime};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSDictionary, NSNumber, NSString};

#[cfg(target_os = "macos")]
pub type Dict = NSDictionary<NSString, AnyObject>;

#[cfg(target_os = "macos")]
pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// Toll-free bridge a CFString to NSString.
#[cfg(target_os = "macos")]
pub fn cf_ns(s: &CFString) -> &NSString {
    unsafe { &*(s as *const CFString as *const NSString) }
}

#[cfg(target_os = "macos")]
pub fn num_i(v: i64) -> Retained<AnyObject> {
    Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_i64(v))))
}
#[cfg(target_os = "macos")]
pub fn num_f(v: f64) -> Retained<AnyObject> {
    Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_f64(v))))
}
#[cfg(target_os = "macos")]
pub fn num_b(v: bool) -> Retained<AnyObject> {
    Retained::into_super(Retained::into_super(Retained::into_super(NSNumber::new_bool(v))))
}
#[cfg(target_os = "macos")]
pub fn obj<T: objc2::Message>(v: &T) -> Retained<AnyObject> {
    unsafe { Retained::retain(v as *const T as *mut AnyObject) }.unwrap()
}

#[cfg(target_os = "macos")]
pub fn dict(pairs: Vec<(&NSString, Retained<AnyObject>)>) -> Retained<Dict> {
    let keys: Vec<&NSString> = pairs.iter().map(|p| p.0).collect();
    let vals: Vec<Retained<AnyObject>> = pairs.iter().map(|p| p.1.clone()).collect();
    NSDictionary::from_retained_objects(&keys, &vals)
}

#[cfg(target_os = "macos")]
pub fn host_now() -> CMTime {
    unsafe { CMClock::host_time_clock().time() }
}

#[cfg(target_os = "macos")]
pub fn secs(t: CMTime) -> f64 {
    unsafe { t.seconds() }
}

/// Run `f` inside an Objective-C autorelease pool (a plain call elsewhere).
#[cfg(target_os = "macos")]
pub fn autoreleasepool<R>(f: impl FnOnce() -> R) -> R {
    objc2::rc::autoreleasepool(|_| f())
}

/// Run `f` inside an Objective-C autorelease pool (a plain call elsewhere).
#[cfg(not(target_os = "macos"))]
pub fn autoreleasepool<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// Wrapper asserting a CoreFoundation/ObjC value may cross threads. All uses
/// are serialized by a Mutex or are documented thread-safe CF types.
pub struct SendBox<T>(pub T);
unsafe impl<T> Send for SendBox<T> {}
unsafe impl<T> Sync for SendBox<T> {}

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}
#[cfg(target_os = "macos")]
pub const PIX_420V: u32 = fourcc(b"420v");
#[cfg(target_os = "macos")]
pub const PIX_BGRA: u32 = fourcc(b"BGRA");

#[cfg(target_os = "macos")]
pub fn make_pool(w: usize, h: usize, pix: u32) -> objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBufferPool> {
    let attrs = dict(vec![
        (cf_ns(unsafe { objc2_core_video::kCVPixelBufferPixelFormatTypeKey }), num_i(pix as i64)),
        (cf_ns(unsafe { objc2_core_video::kCVPixelBufferWidthKey }), num_i(w as i64)),
        (cf_ns(unsafe { objc2_core_video::kCVPixelBufferHeightKey }), num_i(h as i64)),
        (cf_ns(unsafe { objc2_core_video::kCVPixelBufferIOSurfacePropertiesKey }), obj(&*dict(vec![]))),
    ]);
    let attrs_cf: &objc2_core_foundation::CFDictionary =
        unsafe { &*(&*attrs as *const Dict as *const objc2_core_foundation::CFDictionary) };
    let mut pool = std::ptr::null_mut();
    unsafe {
        objc2_core_video::CVPixelBufferPool::create(None, None, Some(attrs_cf), std::ptr::NonNull::from(&mut pool));
        objc2_core_foundation::CFRetained::from_raw(std::ptr::NonNull::new(pool).expect("pixel buffer pool"))
    }
}

/// Local date-time like "2026-10-11 at 11.30.12" (for file names).
pub fn timestamp_name() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02} at {:02}.{:02}.{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is not set"))
}

/// Default folder for recordings and exports (`Glide` inside the user's videos folder).
#[cfg(target_os = "macos")]
pub fn videos_dir() -> PathBuf {
    home().join("Movies").join("Glide")
}

/// Default folder for recordings and exports (`Glide` inside the user's videos folder).
#[cfg(not(target_os = "macos"))]
pub fn videos_dir() -> PathBuf {
    xdg_user_dir("XDG_VIDEOS_DIR").unwrap_or_else(|| home().join("Videos")).join("Glide")
}

/// Config directory for presets and settings.
#[cfg(target_os = "macos")]
pub fn config_dir() -> PathBuf {
    home().join("Library").join("Application Support").join("Glide")
}

/// Config directory for presets and settings (`$XDG_CONFIG_HOME/glide`).
#[cfg(not(target_os = "macos"))]
pub fn config_dir() -> PathBuf {
    xdg_base("XDG_CONFIG_HOME", ".config").join("glide")
}

/// Data directory for models and other app state.
#[cfg(target_os = "macos")]
pub fn data_dir() -> PathBuf {
    config_dir()
}

/// Data directory for models and other app state (`$XDG_DATA_HOME/glide`).
#[cfg(not(target_os = "macos"))]
pub fn data_dir() -> PathBuf {
    xdg_base("XDG_DATA_HOME", ".local/share").join("glide")
}

#[cfg(not(target_os = "macos"))]
fn xdg_base(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

/// Reads `NAME="path"` from `~/.config/user-dirs.dirs`, expanding `$HOME`.
#[cfg(not(target_os = "macos"))]
fn xdg_user_dir(name: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(xdg_base("XDG_CONFIG_HOME", ".config").join("user-dirs.dirs")).ok()?;
    let line = text.lines().find_map(|l| l.trim().strip_prefix(name)?.strip_prefix('='))?;
    let value = line.trim().trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home().join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    (!value.is_empty()).then_some(path)
}
