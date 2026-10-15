//! V4L2 webcam capture: direct ioctl + mmap streaming (no helper process).
//! Requests YUYV and converts to BGRA on the capture thread. Cameras that
//! only deliver MJPEG are rejected: there is no JPEG decoder dependency.

pub use super::audio::{default_microphone, microphones, resolve_mic};
use crate::capture::record::Shared;
use crate::capture::types::{Bgra, clock_secs};
use anyhow::{Context, Result, anyhow, bail};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

/// A capture device or audio source: `id` is the stable selector, `name` is for display.
#[derive(Clone, Debug)]
pub struct Device {
    pub id: String,
    pub name: String,
}

/// Requested capture geometry; the driver may adjust it, and the granted size is used.
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const BUFFER_COUNT: u32 = 4;
/// Poll timeout, so the capture thread notices stop requests promptly.
const POLL_MS: i32 = 200;

const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const FIELD_NONE: u32 = 1;
const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const FOURCC_YUYV: u32 = u32::from_le_bytes([b'Y', b'U', b'Y', b'V']);

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const IOC_RW: u32 = IOC_READ | IOC_WRITE;

/// Linux `_IOC` encoding for the `'V'` (V4L2) ioctl family.
const fn ioc(dir: u32, nr: u32, size: usize) -> libc::c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr) as libc::c_ulong
}

const VIDIOC_QUERYCAP: libc::c_ulong = ioc(IOC_READ, 0, size_of::<V4l2Capability>());
const VIDIOC_S_FMT: libc::c_ulong = ioc(IOC_RW, 5, size_of::<V4l2Format>());
const VIDIOC_REQBUFS: libc::c_ulong = ioc(IOC_RW, 8, size_of::<V4l2RequestBuffers>());
const VIDIOC_QUERYBUF: libc::c_ulong = ioc(IOC_RW, 9, size_of::<V4l2Buffer>());
const VIDIOC_QBUF: libc::c_ulong = ioc(IOC_RW, 15, size_of::<V4l2Buffer>());
const VIDIOC_DQBUF: libc::c_ulong = ioc(IOC_RW, 17, size_of::<V4l2Buffer>());
const VIDIOC_STREAMON: libc::c_ulong = ioc(IOC_WRITE, 18, size_of::<u32>());
const VIDIOC_STREAMOFF: libc::c_ulong = ioc(IOC_WRITE, 19, size_of::<u32>());

// Kernel ABI mirrors (linux/videodev2.h, 64-bit layout). Sizes are checked below.
#[allow(dead_code)]
#[repr(C)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[allow(dead_code)]
#[repr(C)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format`: the union starts at offset 8 (8-byte aligned), 208 bytes total.
#[allow(dead_code)]
#[repr(C)]
struct V4l2Format {
    kind: u32,
    _pad: u32,
    pix: V4l2PixFormat,
    _rest: [u8; 152],
}

#[allow(dead_code)]
#[repr(C)]
struct V4l2RequestBuffers {
    count: u32,
    kind: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

/// `struct v4l2_buffer`: `m` is a union (offset/userptr/planes/fd); only `offset` is used.
#[allow(dead_code)]
#[repr(C)]
struct V4l2Buffer {
    index: u32,
    kind: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: [i64; 2],
    timecode: [u32; 4],
    sequence: u32,
    memory: u32,
    m: u64,
    length: u32,
    reserved2: u32,
    request_fd: u32,
}

const _: () = {
    assert!(size_of::<V4l2Capability>() == 104);
    assert!(size_of::<V4l2Format>() == 208);
    assert!(size_of::<V4l2RequestBuffers>() == 20);
    assert!(size_of::<V4l2Buffer>() == 88);
};

/// Zero-filled ioctl argument. All ABI structs hold only integers and integer arrays,
/// for which all-zero bytes are a valid value.
fn zeroed<T>() -> T {
    // SAFETY: every type passed here is plain integer data (see above).
    unsafe { std::mem::zeroed() }
}

/// Issue a V4L2 ioctl, retrying on EINTR.
fn xioctl<T>(fd: i32, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: `arg` is a valid, exclusively borrowed ABI struct of the size the request encodes.
        let r = unsafe { libc::ioctl(fd, req as _, arg as *mut T) };
        if r == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Card name if the node supports video capture (device caps when the driver reports them).
fn capture_card(file: &File) -> Option<String> {
    let mut cap: V4l2Capability = zeroed();
    xioctl(file.as_raw_fd(), VIDIOC_QUERYCAP, &mut cap).ok()?;
    let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 {
        cap.device_caps
    } else {
        cap.capabilities
    };
    (caps & CAP_VIDEO_CAPTURE != 0).then(|| cstr(&cap.card))
}

/// Video capture devices in `/dev/videoN` order. `id` is the node path, `name` the card name.
pub fn cameras() -> Vec<Device> {
    let Ok(dir) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut nodes: Vec<(u32, String)> = dir
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            let n = name.strip_prefix("video")?.parse::<u32>().ok()?;
            Some((n, name))
        })
        .collect();
    nodes.sort();
    nodes
        .into_iter()
        .filter_map(|(_, node)| {
            let path = format!("/dev/{node}");
            let file = File::open(&path).ok()?;
            let card = capture_card(&file)?;
            let name = if card.is_empty() { path.clone() } else { card };
            Some(Device { id: path, name })
        })
        .collect()
}

/// No iPhone continuity cameras on Linux (AVFoundation-only).
pub fn ios_devices() -> Vec<Device> {
    Vec::new()
}

/// AVFoundation screen-capture devices are macOS-only; nothing to enable on Linux.
pub fn enable_screen_capture_devices() {}

/// The lid-closed built-in mic auto-off is macOS-only (IOKit); never applies on Linux.
pub fn builtin_mic_off(_: Option<&str>) -> bool {
    false
}

/// A memory-mapped, streaming V4L2 capture device. Teardown stops the stream and unmaps.
struct Stream {
    file: File,
    /// (address, length) of each mmap'd buffer, indexed by V4L2 buffer index.
    bufs: Vec<(usize, usize)>,
    w: usize,
    h: usize,
    stride: usize,
}

impl Stream {
    fn open(path: &str) -> Result<Stream> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open {path}"))?;
        let fd = file.as_raw_fd();

        let mut fmt: V4l2Format = zeroed();
        fmt.kind = BUF_TYPE_VIDEO_CAPTURE;
        fmt.pix.width = WIDTH;
        fmt.pix.height = HEIGHT;
        fmt.pix.pixelformat = FOURCC_YUYV;
        fmt.pix.field = FIELD_NONE;
        xioctl(fd, VIDIOC_S_FMT, &mut fmt).context("set camera format")?;
        if fmt.pix.pixelformat != FOURCC_YUYV {
            bail!("{path} does not deliver YUYV (MJPEG-only cameras are unsupported)");
        }
        let w = fmt.pix.width as usize;
        let h = fmt.pix.height as usize;
        let stride = match fmt.pix.bytesperline as usize {
            0 => w * 2,
            s => s,
        };
        if w == 0 || w % 2 != 0 || stride < w * 2 || h == 0 {
            bail!("{path}: unusable frame size {w}x{h} (stride {stride})");
        }

        // Record each mapping as it is made so Drop unmaps whatever succeeded.
        let mut stream = Stream {
            file,
            bufs: Vec::new(),
            w,
            h,
            stride,
        };

        let mut req: V4l2RequestBuffers = zeroed();
        req.count = BUFFER_COUNT;
        req.kind = BUF_TYPE_VIDEO_CAPTURE;
        req.memory = MEMORY_MMAP;
        xioctl(fd, VIDIOC_REQBUFS, &mut req).context("request camera buffers")?;
        if req.count < 2 {
            bail!("{path}: driver granted only {} buffer(s)", req.count);
        }

        for index in 0..req.count {
            let mut b: V4l2Buffer = zeroed();
            b.index = index;
            b.kind = BUF_TYPE_VIDEO_CAPTURE;
            b.memory = MEMORY_MMAP;
            xioctl(fd, VIDIOC_QUERYBUF, &mut b).context("query camera buffer")?;
            let len = b.length as usize;
            if len < stride * h {
                bail!("{path}: buffer {index} holds {len} bytes, frame needs {}", stride * h);
            }
            // SAFETY: mapping a V4L2 MMAP buffer of `len` bytes at its driver-given offset.
            let addr = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    len,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    fd,
                    b.m as u32 as libc::off_t,
                )
            };
            if addr == libc::MAP_FAILED {
                bail!("mmap camera buffer {index}: {}", io::Error::last_os_error());
            }
            stream.bufs.push((addr as usize, len));
            xioctl(fd, VIDIOC_QBUF, &mut b).context("queue camera buffer")?;
        }

        let mut kind = BUF_TYPE_VIDEO_CAPTURE;
        xioctl(fd, VIDIOC_STREAMON, &mut kind).context("start camera stream")?;
        Ok(stream)
    }

    /// Wait up to POLL_MS for one frame. Returns its capture time and BGRA pixels.
    fn frame(&mut self) -> Result<Option<(f64, Bgra)>> {
        let fd = self.file.as_raw_fd();
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the lifetime of the call.
        let n = unsafe { libc::poll(&mut pfd, 1, POLL_MS) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            bail!("poll camera: {e}");
        }
        if n == 0 {
            return Ok(None);
        }

        let mut b: V4l2Buffer = zeroed();
        b.kind = BUF_TYPE_VIDEO_CAPTURE;
        b.memory = MEMORY_MMAP;
        xioctl(fd, VIDIOC_DQBUF, &mut b).context("dequeue camera frame")?;
        let pts = clock_secs();
        let &(addr, len) = self
            .bufs
            .get(b.index as usize)
            .ok_or_else(|| anyhow!("driver returned buffer index {}", b.index))?;
        // SAFETY: the buffer is mapped for `len` bytes and is owned by us until QBUF below,
        // so the kernel does not write it while this slice is alive.
        let raw = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
        let px = yuyv_to_bgra(raw, self.w, self.h, self.stride);
        xioctl(fd, VIDIOC_QBUF, &mut b).context("requeue camera buffer")?;
        Ok(Some((
            pts,
            Bgra {
                w: self.w,
                h: self.h,
                px,
            },
        )))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let mut kind = BUF_TYPE_VIDEO_CAPTURE;
        // Teardown: errors here (e.g. stream never started) are not actionable.
        let _ = xioctl(self.file.as_raw_fd(), VIDIOC_STREAMOFF, &mut kind);
        for &(addr, len) in &self.bufs {
            // SAFETY: unmapping a region this struct mapped in `open`; not used afterwards.
            unsafe {
                libc::munmap(addr as *mut libc::c_void, len);
            }
        }
    }
}

/// Packed YUYV 4:2:2 to BGRA, BT.601 limited range. `w` must be even.
fn yuyv_to_bgra(src: &[u8], w: usize, h: usize, stride: usize) -> Vec<u8> {
    fn clamp(v: i32) -> u8 {
        v.clamp(0, 255) as u8
    }
    let mut out = vec![0u8; w * h * 4];
    for (y, row) in src.chunks_exact(stride).take(h).enumerate() {
        let dst = &mut out[y * w * 4..(y + 1) * w * 4];
        for (x, quad) in row[..w * 2].chunks_exact(4).enumerate() {
            let u = quad[1] as i32 - 128;
            let v = quad[3] as i32 - 128;
            for (k, &luma) in [quad[0], quad[2]].iter().enumerate() {
                let c = luma as i32 - 16;
                let r = (298 * c + 409 * v + 128) >> 8;
                let g = (298 * c - 100 * u - 208 * v + 128) >> 8;
                let b = (298 * c + 516 * u + 128) >> 8;
                dst[(x * 2 + k) * 4..][..4].copy_from_slice(&[clamp(b), clamp(g), clamp(r), 255]);
            }
        }
    }
    out
}

/// A live webcam feeding `Shared::push_cam` from its own thread.
pub struct Camera {
    pub name: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Camera {
    /// `sel`: None = first camera; Some = exact device path, else case-insensitive name substring.
    pub fn start(sel: Option<&str>, shared: Arc<Shared>) -> Result<Camera> {
        let devices = cameras();
        let dev = match sel {
            None => devices.first().ok_or_else(|| anyhow!("no camera found"))?,
            Some(sel) => {
                let lower = sel.to_lowercase();
                devices
                    .iter()
                    .find(|d| d.id == sel)
                    .or_else(|| {
                        devices
                            .iter()
                            .find(|d| d.name.to_lowercase().contains(&lower))
                    })
                    .ok_or_else(|| anyhow!("no camera matches {sel:?} (see `glide devices`)"))?
            }
        };
        let (name, path) = (dev.name.clone(), dev.id.clone());
        let stream = Stream::open(&path)?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || pump(stream, &shared, &stop))
        };
        Ok(Camera {
            name,
            stop,
            thread: Some(thread),
        })
    }
}

fn pump(mut stream: Stream, shared: &Shared, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) && !shared.stopped() {
        match stream.frame() {
            Ok(Some((pts, frame))) => shared.push_cam(pts, frame),
            Ok(None) => {}
            Err(e) => {
                shared.fail(format!("camera: {e:#}"));
                break;
            }
        }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            if t.join().is_err() {
                eprintln!("glide: camera thread panicked");
            }
        }
    }
}
