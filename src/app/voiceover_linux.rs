//! Voice-over recording on Linux: a cpal input stream feeds 16-bit stereo PCM
//! to a writer thread that streams a WAV file into the package's `recording/`
//! directory. The file is named like the mac recording, with a `.wav`
//! extension, and `VoiceRecorder::file` carries that name.

use std::fs::{self, File};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

/// An in-progress voice-over recording; `stop` finishes the file.
pub struct VoiceRecorder {
    stream: cpal::Stream,
    writer: JoinHandle<std::io::Result<u64>>,
    rate: u32,
    started: Instant,
    /// File name inside the package's `recording/` directory (`.wav`).
    pub file: String,
    /// Source-time position where the recording starts, in seconds.
    pub start_source: f64,
}

impl VoiceRecorder {
    /// Starts recording the default microphone into `package/recording/<file>`.
    /// The stored name uses a `.wav` extension in place of the caller's.
    pub fn start(package: &Path, file: &str, start_source: f64) -> Result<Self> {
        let host = cpal::default_host();
        let device = host.default_input_device().context("no microphone")?;
        let supported = device
            .default_input_config()
            .context("microphone has no input configuration")?;
        let format = supported.sample_format();
        let config = supported.config();
        let rate = config.sample_rate;

        let dir = package.join("recording");
        fs::create_dir_all(&dir)?;
        let file = Path::new(file).with_extension("wav").to_string_lossy().into_owned();
        let path = dir.join(&file);

        let (tx, rx) = mpsc::channel::<Vec<i16>>();
        let stream = match format {
            SampleFormat::F32 => input::<f32>(&device, &config, tx)?,
            SampleFormat::I16 => input::<i16>(&device, &config, tx)?,
            SampleFormat::I32 => input::<i32>(&device, &config, tx)?,
            other => bail!("unsupported microphone format {other:?}"),
        };
        let writer = {
            let path = path.clone();
            thread::Builder::new()
                .name("voiceover-writer".into())
                .spawn(move || write_stream(rx, &path, rate))?
        };
        let started = Instant::now();
        if let Err(e) = stream.play() {
            drop(stream);
            let _ = writer.join();
            let _ = fs::remove_file(&path);
            return Err(e).context("starting the microphone");
        }
        Ok(Self { stream, writer, rate, started, file, start_source })
    }

    /// Seconds recorded so far (wall clock).
    pub fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// Stops recording and finalizes the WAV file. Returns the duration in
    /// **milliseconds**, counted from the frames written.
    pub fn stop(self) -> f64 {
        let Self { stream, writer, rate, started, .. } = self;
        drop(stream);
        match writer.join() {
            Ok(Ok(frames)) => frames as f64 / rate as f64 * 1000.0,
            Ok(Err(e)) => {
                eprintln!("glide: voice-over write failed: {e}");
                started.elapsed().as_secs_f64() * 1000.0
            }
            Err(_) => {
                eprintln!("glide: voice-over writer panicked");
                started.elapsed().as_secs_f64() * 1000.0
            }
        }
    }
}

/// Builds a paused input stream that converts each device sample to i16 and
/// forwards stereo chunks to the writer. Mono input is duplicated to both
/// channels; channels beyond the first two are dropped.
fn input<T>(device: &cpal::Device, config: &cpal::StreamConfig, tx: Sender<Vec<i16>>) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let stream = device.build_input_stream(
        config.clone(),
        move |data: &[T], _: &_| {
            let mut out = Vec::with_capacity(data.len() / channels * 2);
            for frame in data.chunks_exact(channels) {
                let l = to_i16(frame[0]);
                let r = if channels > 1 { to_i16(frame[1]) } else { l };
                out.push(l);
                out.push(r);
            }
            // A closed receiver means the writer has stopped after an error.
            let _ = tx.send(out);
        },
        |e| eprintln!("glide: voice-over mic: {e}"),
        None,
    )?;
    Ok(stream)
}

fn to_i16<T>(s: T) -> i16
where
    f32: FromSample<T>,
{
    (<f32 as FromSample<T>>::from_sample_(s).clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Streams samples from `rx` into a 16-bit stereo WAV at `path`, then patches
/// the RIFF and data sizes. Returns the number of stereo frames written.
fn write_stream(rx: Receiver<Vec<i16>>, path: &Path, rate: u32) -> std::io::Result<u64> {
    let mut file = BufWriter::new(File::create(path)?);
    file.write_all(&header(rate, 0))?;
    let mut data_bytes: u64 = 0;
    for chunk in rx {
        let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
        file.write_all(&bytes)?;
        data_bytes += bytes.len() as u64;
    }
    file.flush()?;
    let mut file = file.into_inner().map_err(|e| e.into_error())?;
    let data = data_bytes as u32;
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&(36 + data).to_le_bytes())?;
    file.seek(SeekFrom::Start(40))?;
    file.write_all(&data.to_le_bytes())?;
    Ok(data_bytes / 4)
}

/// Canonical 44-byte PCM WAV header: 16-bit stereo at `rate`.
fn header(rate: u32, data_bytes: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_bytes).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&2u16.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 4).to_le_bytes());
    h[32..34].copy_from_slice(&4u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    h
}
