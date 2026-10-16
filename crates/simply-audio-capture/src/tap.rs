//! The output tap (macOS 14.2+): capture what the Mac *plays* on one output
//! device. No BlackHole, no Multi-Output Device.
//!
//! A tap of every process on the device, mixed down to stereo and **not
//! muted** (the room keeps hearing it), wrapped in a **private** aggregate
//! device that this process reads like an input. Both are created by
//! [`OutputTap::start`] and destroyed when the [`OutputTap`] drops, and by
//! coreaudiod when the process dies: a private tap and aggregate belong to
//! the process that made them.
//!
//! The IOProc maps the tap's frames to the requested width and hands them to
//! the [`Feed`] (which resamples, if the device runs at another rate). The
//! tap runs at the device's own rate.

use std::ffi::{c_void, CStr};
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_core_audio::*;
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp, AudioTimeStampFlags};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};

use crate::{Error, ErrorKind, Feed, Permission, Sink};

type OSStatus = i32;

/// The name the private aggregate device carries (visible only to this
/// process).
const AGGREGATE_NAME: &str = "simply-audio-capture tap";

/// A CoreAudio device: what it has, and whether it's the default output.
#[derive(Clone, Debug)]
pub(crate) struct Device {
    pub name: String,
    pub uid: String,
    pub inputs: bool,
    pub outputs: bool,
    pub is_default_output: bool,
}

/// Every audio device, inputs and outputs alike.
pub(crate) fn list_devices() -> Result<Vec<Device>, Error> {
    let default: AudioObjectID = get(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    )?;
    let mut devices = Vec::new();
    for id in get_vec::<AudioObjectID>(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
    )? {
        let has = |scope| {
            !get_vec::<AudioObjectID>(id, kAudioDevicePropertyStreams, scope)
                .unwrap_or_default()
                .is_empty()
        };
        let (inputs, outputs) =
            (has(kAudioObjectPropertyScopeInput), has(kAudioObjectPropertyScopeOutput));
        if !inputs && !outputs {
            continue;
        }
        let (Ok(name), Ok(uid)) = (
            get_string(id, kAudioObjectPropertyName),
            get_string(id, kAudioDevicePropertyDeviceUID),
        ) else {
            continue;
        };
        devices.push(Device { name, uid, inputs, outputs, is_default_output: id == default });
    }
    Ok(devices)
}

/// The output device `name` exactly, or the default output for `""`.
fn find_output(name: &str) -> Result<Device, Error> {
    let outputs: Vec<Device> = list_devices()?.into_iter().filter(|d| d.outputs).collect();
    let found = match name {
        "" => outputs.iter().find(|o| o.is_default_output),
        n => outputs.iter().find(|o| o.name == n),
    };
    found.cloned().ok_or_else(|| {
        let names: Vec<&str> = outputs.iter().map(|o| o.name.as_str()).collect();
        match name {
            "" => Error::new(ErrorKind::NotFound, "there is no default output device"),
            n => Error::new(
                ErrorKind::NotFound,
                format!("output device {n:?} not found; available: {}", names.join(", ")),
            ),
        }
    })
}

/// Read (never request or change) the TCC state for audio capture. TCC has
/// no public API for this one; `TCCAccessPreflight` is the read-only check
/// the system's own tools use. Unknown if it's missing.
pub(crate) fn permission() -> Permission {
    type Preflight = unsafe extern "C" fn(*const c_void, *const c_void) -> i32;
    unsafe {
        let lib = libc::dlopen(
            c"/System/Library/PrivateFrameworks/TCC.framework/TCC".as_ptr(),
            libc::RTLD_LAZY,
        );
        if lib.is_null() {
            return Permission::Unknown;
        }
        let sym = libc::dlsym(lib, c"TCCAccessPreflight".as_ptr());
        if sym.is_null() {
            return Permission::Unknown;
        }
        let preflight: Preflight = std::mem::transmute(sym);
        let service = NSString::from_str("kTCCServiceAudioCapture");
        match preflight(Retained::as_ptr(&service).cast(), std::ptr::null()) {
            0 => Permission::Granted,
            1 => Permission::Denied,
            _ => Permission::Unknown,
        }
    }
}

/// Scratch frames for mapping the tap's width to the requested one.
const MAP_FRAMES: usize = 1024;

/// What the IOProc owns. Boxed and leaked to a raw pointer for the
/// callback's lifetime; [`OutputTap`]'s drop stops the IOProc first, then
/// frees it.
struct Ctx {
    feed: Feed,
    channels: usize,
    rate: f64,
    scratch: Vec<f32>,
    /// mach ticks → ns.
    numer: u64,
    denom: u64,
}

/// A running tap. Dropping it stops the IOProc and destroys the private
/// aggregate device and the tap, in that order.
pub(crate) struct OutputTap {
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    ctx: *mut Ctx,
    pub device: String,
    /// The rate the tap delivers (the device's own nominal rate).
    pub sample_rate: u32,
}

// The raw pointer is only touched by CoreAudio's IO thread between
// AudioDeviceStart and AudioDeviceStop, and freed after the stop.
unsafe impl Send for OutputTap {}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

impl OutputTap {
    /// Tap every process on `device` (`""` = the default output) and start
    /// handing interleaved `channels`-wide frames at `rate` to `sink`.
    pub(crate) fn start(
        device: &str,
        rate: u32,
        channels: usize,
        sink: Sink,
    ) -> Result<OutputTap, Error> {
        let output = find_output(device)?;
        let mut t = OutputTap {
            tap: 0,
            aggregate: 0,
            proc_id: None,
            ctx: std::ptr::null_mut(),
            device: output.name.clone(),
            sample_rate: 0,
        };
        // Any early return drops `t`, which undoes whatever was created.
        unsafe {
            let empty = NSArray::<NSNumber>::new();
            let uid = NSString::from_str(&output.uid);
            let desc = CATapDescription::initExcludingProcesses_andDeviceUID_withStream(
                CATapDescription::alloc(),
                &empty,
                &uid,
                0,
            );
            desc.setName(&NSString::from_str(AGGREGATE_NAME));
            desc.setMixdown(true);
            desc.setMono(false);
            desc.setPrivate(true);
            desc.setMuteBehavior(CATapMuteBehavior::Unmuted);
            check(
                AudioHardwareCreateProcessTap(Some(&desc), &mut t.tap),
                "creating the process tap (macOS 14.2+)",
            )?;

            let tap_uid = desc.UUID().UUIDString();

            let key = |k: &CStr| NSString::from_str(k.to_str().unwrap_or_default());
            let sub_device = NSDictionary::<NSString, AnyObject>::from_slices(
                &[&*key(kAudioSubDeviceUIDKey)],
                &[&*uid as &AnyObject],
            );
            let yes = NSNumber::new_bool(true);
            let sub_tap = NSDictionary::<NSString, AnyObject>::from_slices(
                &[&*key(kAudioSubTapUIDKey), &*key(kAudioSubTapDriftCompensationKey)],
                &[&*tap_uid as &AnyObject, &*yes],
            );
            let agg_uid = NSString::from_str(&format!(
                "simply-audio-capture-tap-{}",
                NSUUID::new().UUIDString()
            ));
            let agg_name = NSString::from_str(AGGREGATE_NAME);
            let subs = NSArray::from_slice(&[&*sub_device]);
            let taps = NSArray::from_slice(&[&*sub_tap]);
            let agg = NSDictionary::<NSString, AnyObject>::from_slices(
                &[
                    &*key(kAudioAggregateDeviceUIDKey),
                    &*key(kAudioAggregateDeviceNameKey),
                    &*key(kAudioAggregateDeviceMainSubDeviceKey),
                    &*key(kAudioAggregateDeviceIsPrivateKey),
                    &*key(kAudioAggregateDeviceTapAutoStartKey),
                    &*key(kAudioAggregateDeviceSubDeviceListKey),
                    &*key(kAudioAggregateDeviceTapListKey),
                ],
                &[&*agg_uid as &AnyObject, &*agg_name, &*uid, &*yes, &*yes, &*subs, &*taps],
            );
            // NSDictionary is toll-free bridged to CFDictionary.
            let cf: &CFDictionary = &*(Retained::as_ptr(&agg) as *const CFDictionary);
            check(
                AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut t.aggregate)),
                "creating the private aggregate device",
            )?;
            // The rate the IOProc runs at is the aggregate's, clocked by the
            // tapped device. (The tap's own format property said 48000 for
            // speakers running at 44100 — measured — so it isn't used.)
            let device_rate: f64 = get(
                t.aggregate,
                kAudioDevicePropertyNominalSampleRate,
                kAudioObjectPropertyScopeGlobal,
            )?;
            t.sample_rate = device_rate.round() as u32;

            let mut tb = MachTimebaseInfo { numer: 1, denom: 1 };
            if mach_timebase_info(&mut tb) != 0 || tb.denom == 0 {
                tb = MachTimebaseInfo { numer: 1, denom: 1 };
            }
            let channels = channels.max(1);
            t.ctx = Box::into_raw(Box::new(Ctx {
                feed: Feed::new(sink, channels, t.sample_rate, rate),
                channels,
                rate: device_rate,
                scratch: vec![0.0; MAP_FRAMES * channels],
                numer: tb.numer as u64,
                denom: tb.denom as u64,
            }));
            check(
                AudioDeviceCreateIOProcID(
                    t.aggregate,
                    Some(io_proc),
                    t.ctx.cast(),
                    NonNull::from(&mut t.proc_id),
                ),
                "creating the tap IOProc",
            )?;
            check(AudioDeviceStart(t.aggregate, t.proc_id), "starting the tap")?;
        }
        Ok(t)
    }
}

impl Drop for OutputTap {
    fn drop(&mut self) {
        unsafe {
            if self.proc_id.is_some() {
                AudioDeviceStop(self.aggregate, self.proc_id);
                AudioDeviceDestroyIOProcID(self.aggregate, self.proc_id);
            }
            if !self.ctx.is_null() {
                drop(Box::from_raw(self.ctx));
            }
            if self.aggregate != 0 {
                AudioHardwareDestroyAggregateDevice(self.aggregate);
            }
            if self.tap != 0 {
                AudioHardwareDestroyProcessTap(self.tap);
            }
        }
    }
}

/// The real-time callback: the tap's frames, mapped to `channels` wide, into
/// the feed. No allocation, locks, logging or I/O. The tap is the
/// aggregate's last input stream (a tapped device's own inputs, if it has
/// any, come first).
unsafe extern "C-unwind" fn io_proc(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> OSStatus {
    let ctx = &mut *(client as *mut Ctx);
    let list = input.as_ptr();
    let n = (*list).mNumberBuffers as usize;
    if n == 0 {
        return 0;
    }
    let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), n);
    let buf = &buffers[n - 1];
    let src_ch = buf.mNumberChannels.max(1) as usize;
    if buf.mData.is_null() {
        return 0;
    }
    let time = input_time.as_ref();
    let ns = if time.mFlags.contains(AudioTimeStampFlags::HostTimeValid) {
        (time.mHostTime as u128 * ctx.numer as u128 / ctx.denom as u128) as u64
    } else {
        0
    };
    let samples =
        std::slice::from_raw_parts(buf.mData as *const f32, buf.mDataByteSize as usize / 4);
    let frames = samples.len() / src_ch;
    if src_ch == ctx.channels {
        ctx.feed.push(&samples[..frames * src_ch], ns);
        return 0;
    }
    let ch = ctx.channels;
    let mut done = 0;
    while done < frames {
        let count = (frames - done).min(MAP_FRAMES);
        for f in 0..count {
            let frame = &samples[(done + f) * src_ch..(done + f + 1) * src_ch];
            for c in 0..ch {
                ctx.scratch[f * ch + c] = frame[c.min(src_ch - 1)];
            }
        }
        let at = if ns == 0 { 0 } else { ns + (done as f64 * 1e9 / ctx.rate) as u64 };
        ctx.feed.push(&ctx.scratch[..count * ch], at);
        done += count;
    }
    0
}

fn check(status: OSStatus, what: &str) -> Result<(), Error> {
    if status != 0 {
        let fourcc = status.to_be_bytes();
        let code = if fourcc.iter().all(|b| b.is_ascii_graphic()) {
            format!("'{}'", String::from_utf8_lossy(&fourcc))
        } else {
            status.to_string()
        };
        return Err(Error::new(ErrorKind::Device, format!("{what}: OSStatus {code}")));
    }
    Ok(())
}

fn address(
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// One fixed-size property value.
fn get<T: Copy>(
    obj: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Result<T, Error> {
    let addr = address(selector, scope);
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut size = std::mem::size_of::<T>() as u32;
    unsafe {
        check(
            AudioObjectGetPropertyData(
                obj,
                NonNull::from(&addr),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::new_unchecked(value.as_mut_ptr().cast()),
            ),
            "reading an audio property",
        )?;
        Ok(value.assume_init())
    }
}

/// An array property (device lists, stream lists).
fn get_vec<T: Copy + Default>(
    obj: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Result<Vec<T>, Error> {
    let addr = address(selector, scope);
    let mut size = 0u32;
    unsafe {
        check(
            AudioObjectGetPropertyDataSize(
                obj,
                NonNull::from(&addr),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
            ),
            "sizing an audio property",
        )?;
        let mut v = vec![T::default(); size as usize / std::mem::size_of::<T>()];
        if v.is_empty() {
            return Ok(v);
        }
        check(
            AudioObjectGetPropertyData(
                obj,
                NonNull::from(&addr),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::new_unchecked(v.as_mut_ptr().cast()),
            ),
            "reading an audio property",
        )?;
        v.truncate(size as usize / std::mem::size_of::<T>());
        Ok(v)
    }
}

/// A CFString property (names, UIDs); the returned reference is owned.
fn get_string(obj: AudioObjectID, selector: AudioObjectPropertySelector) -> Result<String, Error> {
    let ptr: *mut NSString = get(obj, selector, kAudioObjectPropertyScopeGlobal)?;
    let s = unsafe { Retained::from_raw(ptr) }
        .ok_or_else(|| Error::new(ErrorKind::Device, "null string property"))?;
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Enumeration needs no permission and finds the default output.
    #[test]
    fn the_default_output_is_listed() {
        let devices = list_devices().unwrap();
        assert!(devices.iter().filter(|o| o.is_default_output).count() <= 1);
        if devices.iter().any(|d| d.outputs) {
            assert!(find_output("").is_ok());
        }
        assert!(find_output("no such device, surely").is_err());
    }

    /// Creates a real tap on the default output and destroys it: the private
    /// aggregate must be gone afterwards. Ignored by default because it
    /// touches the machine's audio (and the very first time may raise the
    /// permission prompt): `cargo test -p simply-audio-capture -- --ignored`.
    #[test]
    #[ignore]
    fn a_tap_leaves_no_aggregate_device_behind() {
        let ours = || list_devices().unwrap().iter().filter(|o| o.name == AGGREGATE_NAME).count();
        eprintln!("permission: {:?}", permission());
        let tap = OutputTap::start("", 48_000, 2, Box::new(|_: &[f32], _: u64| {})).unwrap();
        eprintln!("tapping {:?} at {} Hz", tap.device, tap.sample_rate);
        assert_eq!(ours(), 1, "the private aggregate is visible to its own process");
        drop(tap);
        assert_eq!(ours(), 0);
    }
}
