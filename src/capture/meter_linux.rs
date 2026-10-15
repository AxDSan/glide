//! Microphone input level meter over PipeWire, mirroring the macOS `MicMeter`.

use super::audio::{TICK, connect, input_stream, resolve_mic};
use anyhow::{Result, bail};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

/// Peak-level meter on one microphone. Dropping it stops the PipeWire thread.
pub struct MicMeter {
    level: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MicMeter {
    /// Start metering `name` (a mic selector, see `resolve_mic`); None = default source.
    pub fn start(name: Option<&str>) -> Result<MicMeter> {
        let target = name.map(resolve_mic).transpose()?;
        let level = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = {
            let (level, stop) = (level.clone(), stop.clone());
            std::thread::spawn(move || {
                if let Err(e) = run(target, level, stop, &ready_tx) {
                    let _ = ready_tx.send(Err(format!("{e:#}")));
                }
            })
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(MicMeter {
                level,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                bail!("microphone: {e}")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::Relaxed);
                bail!("microphone timed out")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("microphone meter thread exited during setup")
            }
        }
    }

    /// Peak sample magnitude since the previous call, then reset.
    pub fn take_level(&self) -> f32 {
        f32::from_bits(self.level.swap(0, Ordering::Relaxed))
    }
}

impl Drop for MicMeter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            if t.join().is_err() {
                eprintln!("glide: microphone meter thread panicked");
            }
        }
    }
}

fn run(
    target: Option<String>,
    level: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    ready: &mpsc::Sender<Result<(), String>>,
) -> Result<()> {
    let (mainloop, _context, core) = connect()?;
    // Non-negative floats order like their bit patterns, so fetch_max tracks the peak.
    let _stream = input_stream(
        &core,
        "glide level meter",
        target.as_deref(),
        false,
        level,
        |level: &mut Arc<AtomicU32>, _pts, frames: &[[f32; 2]]| {
            let peak = frames
                .iter()
                .flatten()
                .fold(0f32, |m, s| m.max(s.abs()));
            level.fetch_max(peak.to_bits(), Ordering::Relaxed);
        },
        |_, msg: String| eprintln!("glide: microphone meter: {msg}"),
    )?;

    let ml = mainloop.clone();
    let tick = mainloop.loop_().add_timer(move |_| {
        if stop.load(Ordering::Relaxed) {
            ml.quit();
        }
    });
    tick.update_timer(Some(TICK), Some(TICK)).into_result()?;
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}
