//! Process taps: what signal each HAL client is actually *producing*.
//!
//! ## Why this exists
//!
//! The poll in `watch` detects `IsRunningOutput` going 0 -> 1. A process that
//! holds an output stream open permanently — `arkaudiod` on this machine does
//! exactly that — never transitions, so audio passing through it is invisible to
//! that detector. A tap measures the signal itself, so a burst is caught on the
//! burst.
//!
//! ## Shape
//!
//! One `CATapDescription` per audio process, all of them listed in a single
//! private aggregate device. The aggregate then presents one mono buffer per tap
//! in tap-list order, so buffer *i* is process *i* and attribution is exact.
//! Measured: 30 taps in one aggregate gave 30 one-channel buffers.
//!
//! ## Privacy
//!
//! The IOProc computes a peak magnitude per buffer and discards the samples. No
//! audio is retained, copied or written anywhere unless `capture` is asked for
//! explicitly, which is a separate entry point.

use crate::bridge::{self, CfOwned, Id};
use crate::event::{Kind, Record};
use crate::hal::{CoreAudio, HalView};
use crate::sys::{self, AudioObjectId, PropertyAddress};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Aggregate devices present one buffer per tap; this bounds the arrays the
/// real-time callback writes into, since it cannot allocate.
pub const MAX_TAPS: usize = 64;

// ---------------------------------------------------------------------------
// Metering state, shared with the real-time callback.
//
// The callback runs on a CoreAudio real-time thread and must not allocate, lock
// or block. It therefore only folds a peak into a static array of atomics; the
// main thread reads and resets them. `fetch_max` on the bit pattern is a correct
// maximum because these are non-negative floats, whose IEEE-754 bit patterns
// order the same way as their values.
// ---------------------------------------------------------------------------

static PEAK_BITS: [AtomicU32; MAX_TAPS] = [const { AtomicU32::new(0) }; MAX_TAPS];
static FRAMES: [AtomicU64; MAX_TAPS] = [const { AtomicU64::new(0) }; MAX_TAPS];
static CALLBACKS: AtomicU64 = AtomicU64::new(0);
static BUFFERS_SEEN: AtomicU32 = AtomicU32::new(0);

extern "C" fn meter_proc(
    _device: AudioObjectId,
    _now: *const sys::AudioTimeStamp,
    input: *const sys::AudioBufferList,
    _input_time: *const sys::AudioTimeStamp,
    _output: *mut sys::AudioBufferList,
    _output_time: *const sys::AudioTimeStamp,
    _client: *mut c_void,
) -> sys::OsStatus {
    CALLBACKS.fetch_add(1, Ordering::Relaxed);
    if input.is_null() {
        return 0;
    }
    // SAFETY: CoreAudio guarantees `input` points to an AudioBufferList whose
    // `number_buffers` buffers follow inline, valid for this call only. We read
    // and never retain.
    unsafe {
        let count = (*input).number_buffers as usize;
        BUFFERS_SEEN.store(count as u32, Ordering::Relaxed);
        let buffers = std::ptr::addr_of!((*input).buffers) as *const sys::AudioBuffer;
        for i in 0..count.min(MAX_TAPS) {
            let buffer = &*buffers.add(i);
            if buffer.data.is_null() {
                continue;
            }
            let samples = buffer.data_byte_size as usize / std::mem::size_of::<f32>();
            let data = buffer.data as *const f32;
            let mut peak = 0f32;
            for s in 0..samples {
                let magnitude = (*data.add(s)).abs();
                if magnitude > peak {
                    peak = magnitude;
                }
            }
            let channels = buffer.number_channels.max(1) as usize;
            FRAMES[i].fetch_add((samples / channels) as u64, Ordering::Relaxed);
            // Discard NaN rather than letting it poison the maximum.
            if peak.is_finite() {
                PEAK_BITS[i].fetch_max(peak.to_bits(), Ordering::Relaxed);
            }
        }
    }
    0
}

/// Read and clear the peak for one buffer index, as a linear magnitude.
fn take_peak(index: usize) -> f32 {
    f32::from_bits(PEAK_BITS[index].swap(0, Ordering::Relaxed))
}

/// Linear magnitude to dBFS. Silence is `None`, which reads better than `-inf`.
pub fn dbfs(magnitude: f32) -> Option<f64> {
    if magnitude <= 0.0 {
        None
    } else {
        Some(20.0 * (magnitude as f64).log10())
    }
}

// ---------------------------------------------------------------------------
// Tap lifecycle
// ---------------------------------------------------------------------------

/// One tap and who it belongs to. The identity is captured when the tap is
/// created, which — like the connect-time capture in `reconcile` — is while the
/// process is certainly still alive.
pub struct Slot {
    pub tap: AudioObjectId,
    pub process_object: AudioObjectId,
    pub pid: i32,
    pub bundle: Option<String>,
    pub exe: Option<String>,
    pub devices: Vec<String>,
}

/// Build a `CATapDescription` for one process object: a mono mixdown, private,
/// and explicitly unmuted so tapping never changes what the process sounds like.
fn tap_description(process_object: AudioObjectId, name: &str) -> Option<Id> {
    let class = bridge::class("CATapDescription")?;
    let number = bridge::ns_number_u32(process_object)?;
    let array = bridge::ns_array(&[number])?;
    // SAFETY: each selector below is sent to an object that responds to it with
    // the signature the transmute in `bridge` declares. Checked against
    // CATapDescription.h.
    unsafe {
        let allocated = bridge::send0(class, bridge::sel("alloc"));
        if allocated.is_null() {
            return None;
        }
        let description = bridge::send1_id(
            allocated,
            bridge::sel("initMonoMixdownOfProcesses:"),
            array,
        );
        if description.is_null() {
            return None;
        }
        if let Some(cf_name) = bridge::cf_string(name) {
            // CFStringRef is an NSString*, so it can be passed straight through.
            bridge::send1_id(
                description,
                bridge::sel("setName:"),
                cf_name.get() as Id,
            );
        }
        // Private: do not show up as a system-wide tap object for other apps.
        bridge::send1_bool(description, bridge::sel("setPrivate:"), true);
        // CATapUnmuted = 0. Tapping must never mute what it observes.
        bridge::send1_isize(description, bridge::sel("setMuteBehavior:"), 0);
        Some(description)
    }
}

fn tap_uid(tap: AudioObjectId) -> Option<String> {
    let raw = sys::get::<*const c_void>(tap, &PropertyAddress::global(sys::PROP_TAP_UID)).ok()?;
    let owned = CfOwned::from_create(raw)?;
    bridge::cf_string_to_rust(owned.get())
}

/// A running set of taps inside one aggregate device.
pub struct TapSet {
    aggregate: AudioObjectId,
    io_proc: sys::IoProcId,
    slots: Vec<Slot>,
    format: sys::StreamBasicDescription,
}

impl TapSet {
    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    pub fn format(&self) -> sys::StreamBasicDescription {
        self.format
    }

    pub fn aggregate(&self) -> AudioObjectId {
        self.aggregate
    }

    /// Tap every listed process object and start metering.
    pub fn start(hal: &CoreAudio, process_objects: &[AudioObjectId]) -> Result<Self, String> {
        if !bridge::available() {
            return Err("CATapDescription is missing; process taps need macOS 14.2 or newer"
                .to_string());
        }
        let mut slots = Vec::new();
        let mut uids = Vec::new();
        let mut format = sys::StreamBasicDescription::default();

        for object in process_objects.iter().copied().take(MAX_TAPS) {
            let details = hal.details(object).unwrap_or_default();
            let name = format!("audiowatch tap for pid {}", details.pid);
            let Some(description) = tap_description(object, &name) else {
                continue;
            };
            // SAFETY: `description` is the CATapDescription just built.
            let created = unsafe { sys::create_process_tap(description) };
            // The description is ours; release it either way.
            // SAFETY: `description` came from +alloc/-init, so we own it.
            unsafe { bridge::send0(description, bridge::sel("release")) };
            let tap = match created {
                Ok(t) => t,
                Err(_) => continue,
            };
            let Some(uid) = tap_uid(tap) else {
                sys::destroy_process_tap(tap);
                continue;
            };
            if format.sample_rate == 0.0 {
                if let Ok(f) = sys::get::<sys::StreamBasicDescription>(
                    tap,
                    &PropertyAddress::global(sys::PROP_TAP_FORMAT),
                ) {
                    format = f;
                }
            }
            uids.push(uid);
            slots.push(Slot {
                tap,
                process_object: object,
                pid: details.pid,
                bundle: details.bundle,
                exe: details.exe,
                devices: details.devices,
            });
        }

        if slots.is_empty() {
            return Err("could not create a tap on any audio process".to_string());
        }

        let aggregate = match Self::create_aggregate(&uids) {
            Ok(a) => a,
            Err(e) => {
                for slot in &slots {
                    sys::destroy_process_tap(slot.tap);
                }
                return Err(e);
            }
        };

        let io_proc = match sys::create_io_proc(aggregate, meter_proc) {
            Ok(p) => p,
            Err(status) => {
                sys::destroy_aggregate_device(aggregate);
                for slot in &slots {
                    sys::destroy_process_tap(slot.tap);
                }
                return Err(format!("cannot install an IO callback: status {status}"));
            }
        };

        // Clear any stale metering from a previous set before samples arrive.
        for i in 0..MAX_TAPS {
            PEAK_BITS[i].store(0, Ordering::Relaxed);
            FRAMES[i].store(0, Ordering::Relaxed);
        }

        let status = sys::device_start(aggregate, io_proc);
        if status != 0 {
            sys::destroy_io_proc(aggregate, io_proc);
            sys::destroy_aggregate_device(aggregate);
            for slot in &slots {
                sys::destroy_process_tap(slot.tap);
            }
            return Err(format!("cannot start the tap device: status {status}"));
        }

        Ok(Self { aggregate, io_proc, slots, format })
    }

    fn create_aggregate(tap_uids: &[String]) -> Result<AudioObjectId, String> {
        // Keys from AudioHardware.h.
        let k_name = bridge::cf_string("name").ok_or("cf")?;
        let k_uid = bridge::cf_string("uid").ok_or("cf")?;
        let k_private = bridge::cf_string("private").ok_or("cf")?;
        let k_stacked = bridge::cf_string("stacked").ok_or("cf")?;
        let k_tap_autostart = bridge::cf_string("tapautostart").ok_or("cf")?;
        let k_subdevices = bridge::cf_string("subdevices").ok_or("cf")?;
        let k_taps = bridge::cf_string("taps").ok_or("cf")?;

        let name = bridge::cf_string("audiowatch tap aggregate").ok_or("cf")?;
        let uid = bridge::cf_string(&format!("audiowatch-taps-{}", std::process::id()))
            .ok_or("cf")?;
        let yes = bridge::cf_bool_as_number(true).ok_or("cf")?;
        let no = bridge::cf_bool_as_number(false).ok_or("cf")?;
        let empty = bridge::cf_array(&[]).ok_or("cf")?;

        // Each tap is a one-entry dictionary {"uid": <tap uid>}. The CFStrings and
        // dictionaries must outlive the array, so they are kept in these vectors.
        let mut keepalive: Vec<CfOwned> = Vec::new();
        let mut entries: Vec<bridge::CfTypeRef> = Vec::new();
        for tap_uid in tap_uids {
            let value = bridge::cf_string(tap_uid).ok_or("cf")?;
            let entry = bridge::cf_dictionary(&[(k_uid.get(), value.get())]).ok_or("cf")?;
            entries.push(entry.get());
            keepalive.push(value);
            keepalive.push(entry);
        }
        let taps = bridge::cf_array(&entries).ok_or("cf")?;

        let description = bridge::cf_dictionary(&[
            (k_name.get(), name.get()),
            (k_uid.get(), uid.get()),
            (k_private.get(), yes.get()),
            (k_stacked.get(), no.get()),
            (k_tap_autostart.get(), yes.get()),
            (k_subdevices.get(), empty.get()),
            (k_taps.get(), taps.get()),
        ])
        .ok_or("cf")?;

        // SAFETY: `description` is a live CFDictionary of the documented shape.
        unsafe { sys::create_aggregate_device(description.get()) }
            .map_err(|status| format!("cannot create the tap aggregate device: status {status}"))
    }

    /// How many buffers the callback is actually being handed. If this does not
    /// match the tap count, attribution by index would be wrong.
    pub fn buffers_seen(&self) -> u32 {
        BUFFERS_SEEN.load(Ordering::Relaxed)
    }

    pub fn callbacks(&self) -> u64 {
        CALLBACKS.load(Ordering::Relaxed)
    }

    pub fn frames(&self, index: usize) -> u64 {
        FRAMES.get(index).map_or(0, |f| f.load(Ordering::Relaxed))
    }
}

impl Drop for TapSet {
    fn drop(&mut self) {
        sys::device_stop(self.aggregate, self.io_proc);
        sys::destroy_io_proc(self.aggregate, self.io_proc);
        sys::destroy_aggregate_device(self.aggregate);
        for slot in &self.slots {
            sys::destroy_process_tap(slot.tap);
        }
    }
}

// ---------------------------------------------------------------------------
// Burst detection
// ---------------------------------------------------------------------------

/// One process's in-progress burst of signal.
#[derive(Default, Clone)]
struct Burst {
    active: bool,
    started_ms: i64,
    last_above_ms: i64,
    peak: f32,
}

/// Turns a stream of per-tap peaks into "process X put N dBFS on a bus for M ms".
pub struct BurstDetector {
    /// Signal at or above this counts as a burst.
    pub threshold_dbfs: f64,
    /// A burst ends once it has been quiet for this long.
    pub hold_ms: i64,
    bursts: Vec<Burst>,
}

/// What a completed burst says.
#[derive(Debug, Clone, PartialEq)]
pub struct SignalBurst {
    pub index: usize,
    pub started_ms: i64,
    pub duration_ms: u64,
    pub peak_dbfs: f64,
}

impl BurstDetector {
    pub fn new(threshold_dbfs: f64, hold_ms: i64) -> Self {
        Self { threshold_dbfs, hold_ms, bursts: vec![Burst::default(); MAX_TAPS] }
    }

    /// Feed one observation for one tap. Returns a burst when one has just ended.
    pub fn observe(&mut self, index: usize, peak: f32, now_ms: i64) -> Option<SignalBurst> {
        if index >= self.bursts.len() {
            return None;
        }
        let above = dbfs(peak).is_some_and(|db| db >= self.threshold_dbfs);
        let burst = &mut self.bursts[index];

        if above {
            if !burst.active {
                burst.active = true;
                burst.started_ms = now_ms;
                burst.peak = 0.0;
            }
            burst.last_above_ms = now_ms;
            if peak > burst.peak {
                burst.peak = peak;
            }
            return None;
        }

        if burst.active && now_ms - burst.last_above_ms >= self.hold_ms {
            let finished = SignalBurst {
                index,
                started_ms: burst.started_ms,
                // The burst lasted until the last observation that was above it.
                duration_ms: (burst.last_above_ms - burst.started_ms).max(0) as u64,
                peak_dbfs: dbfs(burst.peak).unwrap_or(f64::NEG_INFINITY),
            };
            *burst = Burst::default();
            return Some(finished);
        }
        None
    }

    /// Bursts still open, so a shutdown can flush them rather than losing them.
    pub fn flush(&mut self, now_ms: i64) -> Vec<SignalBurst> {
        let mut out = Vec::new();
        for index in 0..self.bursts.len() {
            let burst = &mut self.bursts[index];
            if burst.active {
                out.push(SignalBurst {
                    index,
                    started_ms: burst.started_ms,
                    duration_ms: (now_ms - burst.started_ms).max(0) as u64,
                    peak_dbfs: dbfs(burst.peak).unwrap_or(f64::NEG_INFINITY),
                });
                *burst = Burst::default();
            }
        }
        out
    }
}

/// Read every tap's peak and turn whatever finished into records.
pub fn poll_into_records(
    set: &TapSet,
    detector: &mut BurstDetector,
    now_ms: i64,
) -> Vec<Record> {
    let mut records = Vec::new();
    for (index, slot) in set.slots().iter().enumerate() {
        let peak = take_peak(index);
        if let Some(burst) = detector.observe(index, peak, now_ms) {
            records.push(record_for(slot, &burst));
        }
    }
    records
}

/// Build the log record for a finished burst.
pub fn record_for(slot: &Slot, burst: &SignalBurst) -> Record {
    let mut record = Record::new(burst.started_ms, Kind::Signal, slot.pid);
    record.bundle = slot.bundle.clone();
    record.exe = slot.exe.clone();
    record.devices = slot.devices.clone();
    record.peak_dbfs = Some(burst.peak_dbfs);
    record.duration_ms = Some(burst.duration_ms);
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amp(db: f64) -> f32 {
        10f64.powf(db / 20.0) as f32
    }

    #[test]
    fn dbfs_matches_the_levels_used_in_testing() {
        assert_eq!(dbfs(0.0), None, "digital silence is not a level");
        assert_eq!(dbfs(-1.0), None);
        let sixty = dbfs(0.001).expect("finite");
        assert!((sixty - -60.0).abs() < 0.01, "0.001 should be -60 dBFS, got {sixty}");
        let full = dbfs(1.0).expect("finite");
        assert!(full.abs() < 1e-9, "1.0 should be 0 dBFS, got {full}");
    }

    #[test]
    fn amp_and_dbfs_are_inverses() {
        for db in [-100.0, -80.0, -60.0, -42.0, -6.0] {
            let back = dbfs(amp(db)).unwrap();
            assert!((back - db).abs() < 0.01, "{db} round-tripped to {back}");
        }
    }

    #[test]
    fn a_burst_is_reported_once_it_has_been_quiet_for_the_hold_time() {
        let mut d = BurstDetector::new(-100.0, 300);
        // Silence: nothing.
        assert!(d.observe(0, 0.0, 0).is_none());
        // Signal starts at t=100 and runs to t=280.
        assert!(d.observe(0, amp(-42.0), 100).is_none());
        assert!(d.observe(0, amp(-45.0), 200).is_none());
        assert!(d.observe(0, amp(-50.0), 280).is_none());
        // Quiet, but not yet for the hold time.
        assert!(d.observe(0, 0.0, 400).is_none());
        assert!(d.observe(0, 0.0, 500).is_none());
        // Now it has been quiet for 300ms.
        let burst = d.observe(0, 0.0, 580).expect("burst should close");
        assert_eq!(burst.index, 0);
        assert_eq!(burst.started_ms, 100);
        assert_eq!(burst.duration_ms, 180, "the burst ran from 100 to 280");
        assert!((burst.peak_dbfs - -42.0).abs() < 0.01, "peak was {}", burst.peak_dbfs);
    }

    #[test]
    fn a_gap_shorter_than_the_hold_does_not_split_one_burst_in_two() {
        let mut d = BurstDetector::new(-100.0, 300);
        d.observe(0, amp(-40.0), 0);
        assert!(d.observe(0, 0.0, 100).is_none());
        // Signal resumes inside the hold window.
        assert!(d.observe(0, amp(-40.0), 200).is_none());
        let burst = d.observe(0, 0.0, 600).expect("one burst");
        assert_eq!(burst.started_ms, 0);
        assert_eq!(burst.duration_ms, 200);
    }

    #[test]
    fn signal_below_the_threshold_is_ignored() {
        let mut d = BurstDetector::new(-60.0, 100);
        assert!(d.observe(0, amp(-80.0), 0).is_none());
        assert!(d.observe(0, amp(-80.0), 200).is_none());
        assert!(d.flush(300).is_empty(), "nothing should have been open");
    }

    #[test]
    fn each_tap_is_tracked_independently() {
        let mut d = BurstDetector::new(-100.0, 100);
        d.observe(0, amp(-30.0), 0);
        d.observe(5, amp(-70.0), 0);
        let a = d.observe(0, 0.0, 200).expect("tap 0 closes");
        let b = d.observe(5, 0.0, 200).expect("tap 5 closes");
        assert_eq!(a.index, 0);
        assert_eq!(b.index, 5);
        assert!((a.peak_dbfs - -30.0).abs() < 0.01);
        assert!((b.peak_dbfs - -70.0).abs() < 0.01);
    }

    #[test]
    fn flush_reports_a_burst_that_was_still_open_at_shutdown() {
        let mut d = BurstDetector::new(-100.0, 300);
        d.observe(2, amp(-20.0), 1000);
        let open = d.flush(1500);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].index, 2);
        assert_eq!(open[0].duration_ms, 500);
        // And it is no longer open.
        assert!(d.flush(2000).is_empty());
    }

    #[test]
    fn an_out_of_range_index_is_ignored_rather_than_panicking() {
        let mut d = BurstDetector::new(-100.0, 100);
        assert!(d.observe(MAX_TAPS + 10, amp(-10.0), 0).is_none());
    }

    #[test]
    fn a_record_names_the_process_the_level_and_the_duration() {
        let slot = Slot {
            tap: 1,
            process_object: 2,
            pid: 4242,
            bundle: Some("com.rogueamoeba.arkaudiod".into()),
            exe: Some("/Library/Audio/Plug-Ins/HAL/ARK.driver/x/arkaudiod".into()),
            devices: vec!["BlackHole 2ch".into()],
        };
        let burst = SignalBurst { index: 0, started_ms: 1000, duration_ms: 180, peak_dbfs: -42.0 };
        let r = record_for(&slot, &burst);
        assert_eq!(r.kind, Kind::Signal);
        assert_eq!(r.pid, 4242);
        assert_eq!(r.millis, 1000, "the record is stamped when the burst began");
        assert_eq!(r.peak_dbfs, Some(-42.0));
        assert_eq!(r.duration_ms, Some(180));
        let human = r.human();
        assert!(human.contains("BlackHole 2ch"), "{human}");
        assert!(human.contains("-42.0 dBFS"), "{human}");
        assert!(human.contains("180 ms"), "{human}");
    }

    #[test]
    fn the_peak_atomic_is_a_true_maximum_and_resets() {
        PEAK_BITS[3].store(0, Ordering::Relaxed);
        PEAK_BITS[3].fetch_max(0.25f32.to_bits(), Ordering::Relaxed);
        PEAK_BITS[3].fetch_max(0.75f32.to_bits(), Ordering::Relaxed);
        PEAK_BITS[3].fetch_max(0.5f32.to_bits(), Ordering::Relaxed);
        assert_eq!(take_peak(3), 0.75, "bit-pattern fetch_max must order like the floats");
        assert_eq!(take_peak(3), 0.0, "reading clears it for the next window");
    }
}
