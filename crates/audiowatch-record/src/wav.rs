//! The writer side of a take: drain a ring of interleaved `f32` into a 32-bit
//! float WAV.
//!
//! The audio callback may not block, and a file write blocks, so the two meet
//! in an `rtrb` ring: the callback pushes, a writer thread per take drains
//! here, and the file is finalised (header written, closed) before anyone is
//! told where it is.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// A 32-bit float WAV at the device's own rate, being written.
pub struct WavSink {
    path: PathBuf,
    writer: hound::WavWriter<BufWriter<File>>,
    channels: u16,
    samples: u64,
}

impl WavSink {
    /// Create (or overwrite) `path`, creating its folder if it is missing.
    pub fn create(path: &Path, channels: u16, sample_rate: u32) -> Result<WavSink, hound::Error> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        Ok(WavSink {
            path: path.to_path_buf(),
            writer: hound::WavWriter::create(path, spec)?,
            channels,
            samples: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Whole frames written so far.
    pub fn frames(&self) -> u64 {
        self.samples / u64::from(self.channels.max(1))
    }

    /// Append interleaved samples.
    pub fn write(&mut self, interleaved: &[f32]) -> Result<(), hound::Error> {
        for &s in interleaved {
            self.writer.write_sample(s)?;
        }
        self.samples += interleaved.len() as u64;
        Ok(())
    }

    /// Fix up the header and close the file; returns the frames written.
    pub fn finish(self) -> Result<u64, hound::Error> {
        let frames = self.frames();
        self.writer.finalize()?;
        Ok(frames)
    }
}

/// How long a writer sleeps when its ring is empty. Short against a ring that
/// holds seconds, long enough not to spin a core per take.
const IDLE: Duration = Duration::from_millis(5);

/// Drain `ring` into `sink` until `done` is set **and** the ring is empty —
/// so everything the callback pushed before the stream was dropped reaches
/// the file. `written` follows the frames on disk.
pub fn pump(
    ring: &mut rtrb::Consumer<f32>,
    sink: &mut WavSink,
    written: &AtomicU64,
    done: &AtomicBool,
) -> Result<(), hound::Error> {
    let width = sink.channels() as usize;
    loop {
        // Read `done` before looking at the ring: if it was set, everything
        // the producer will ever push is already visible.
        let finishing = done.load(Ordering::Acquire);
        let slots = ring.slots();
        let available = slots - slots % width;
        if available == 0 {
            if finishing {
                return Ok(());
            }
            std::thread::sleep(IDLE);
            continue;
        }
        let chunk = ring.read_chunk(available).expect("slots were counted");
        let (a, b) = chunk.as_slices();
        // A wrap can split a frame across the two halves; written in order,
        // the file still sees whole frames.
        sink.write(a)?;
        sink.write(b)?;
        chunk.commit_all();
        written.store(sink.frames(), Ordering::Relaxed);
    }
}

/// Copy frames `from..to` of the WAV at `source` into a new float WAV at
/// `dest` (same channels and rate), and return how many were copied — fewer
/// than asked if `source` ends first. This is how a bar-quantised take drops
/// its pre-roll and lands on the downbeat's frame.
pub fn trim(source: &Path, dest: &Path, from: u64, to: u64) -> Result<u64, hound::Error> {
    let mut reader = hound::WavReader::open(source)?;
    let spec = reader.spec();
    let width = u64::from(spec.channels.max(1));
    let mut sink = WavSink::create(dest, spec.channels, spec.sample_rate)?;
    let total = u64::from(reader.duration());
    let (from, to) = (from.min(total), to.min(total));
    reader.seek(from as u32)?;
    let mut buf = Vec::with_capacity(4096);
    let mut left = to.saturating_sub(from) * width;
    let mut samples = reader.samples::<f32>();
    while left > 0 {
        buf.clear();
        for s in samples.by_ref().take(left.min(4096) as usize) {
            buf.push(s?);
        }
        if buf.is_empty() {
            break;
        }
        left -= buf.len() as u64;
        sink.write(&buf)?;
    }
    sink.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("simply-audio-device-{}", std::process::id())).join(name)
    }

    fn read_back(path: &Path) -> (hound::WavSpec, Vec<f32>) {
        let mut reader = hound::WavReader::open(path).unwrap();
        let spec = reader.spec();
        (spec, reader.samples::<f32>().map(Result::unwrap).collect())
    }

    #[test]
    fn a_sink_writes_a_float_wav_at_the_devices_rate() {
        let path = scratch("sink.wav");
        let mut sink = WavSink::create(&path, 2, 44_100).unwrap();
        sink.write(&[0.5, -0.25, 0.125, 0.0]).unwrap();
        assert_eq!(sink.finish().unwrap(), 2);
        let (spec, samples) = read_back(&path);
        assert_eq!(spec.sample_rate, 44_100);
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, hound::SampleFormat::Float);
        assert_eq!(samples, vec![0.5, -0.25, 0.125, 0.0]);
    }

    #[test]
    fn trim_keeps_exactly_the_frames_asked_for() {
        let source = scratch("trim-source.wav");
        let mut sink = WavSink::create(&source, 2, 48_000).unwrap();
        let frames: Vec<f32> = (0..100).flat_map(|i| [i as f32, -(i as f32)]).collect();
        sink.write(&frames).unwrap();
        sink.finish().unwrap();
        let dest = scratch("trim-dest.wav");
        assert_eq!(trim(&source, &dest, 30, 70).unwrap(), 40);
        let (spec, samples) = read_back(&dest);
        assert_eq!((spec.channels, spec.sample_rate), (2, 48_000));
        assert_eq!(&samples[..2], &[30.0, -30.0]);
        assert_eq!(samples.len(), 80);
        // Past the end, it copies what there is.
        assert_eq!(trim(&source, &dest, 90, 500).unwrap(), 10);
        assert_eq!(trim(&source, &dest, 500, 600).unwrap(), 0);
    }

    /// The whole writer path: a producer on another thread pushes frames
    /// through a small ring (so it wraps many times, and frames straddle the
    /// wrap), `done` is set after the last push, and every sample reaches the
    /// file in order.
    #[test]
    fn pump_drains_everything_pushed_before_done_in_order() {
        let path = scratch("pump.wav");
        let (mut tx, mut rx) = rtrb::RingBuffer::<f32>::new(7); // odd: frames straddle
        let done = Arc::new(AtomicBool::new(false));
        let total = 1000usize;
        let producer = {
            let done = done.clone();
            std::thread::spawn(move || {
                for i in 0..total {
                    for s in [i as f32 / total as f32, -(i as f32) / total as f32] {
                        while tx.push(s).is_err() {
                            std::thread::yield_now();
                        }
                    }
                }
                done.store(true, Ordering::Release);
            })
        };
        let mut sink = WavSink::create(&path, 2, 48_000).unwrap();
        let written = AtomicU64::new(0);
        pump(&mut rx, &mut sink, &written, &done).unwrap();
        producer.join().unwrap();
        assert_eq!(written.load(Ordering::Relaxed), total as u64);
        assert_eq!(sink.finish().unwrap(), total as u64);

        let (_, samples) = read_back(&path);
        assert_eq!(samples.len(), total * 2);
        for (i, frame) in samples.chunks_exact(2).enumerate() {
            let v = i as f32 / total as f32;
            assert_eq!(frame, [v, -v], "frame {i}");
        }
    }
}
