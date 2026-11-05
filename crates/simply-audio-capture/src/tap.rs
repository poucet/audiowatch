//! Process taps (macOS 14.2+): capture what the Mac *plays*, either on one
//! output device ([`OutputTap::start`]; [`DefaultTap`] keeps one on
//! whatever the default output is) or by chosen processes
//! ([`Kind::Processes`], which [`Following`] keeps pointed at the apps it
//! follows). No BlackHole, no Multi-Output Device.
//!
//! A tap mixed down to stereo (by default **not muted**: the room keeps
//! hearing it), wrapped in a **private** aggregate device that this process
//! reads like an input. Both are created by [`OutputTap::open`] and
//! destroyed when the [`OutputTap`] drops, and by coreaudiod when the
//! process dies: a private tap and aggregate belong to the process that
//! made them.
//!
//! The IOProc maps the tap's frames to the requested width and hands them to
//! the [`Feed`] (which resamples, if the device runs at another rate). The
//! tap runs at its clock device's own rate.

use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_core_audio::*;
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp, AudioTimeStampFlags};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};

use crate::apps::{
    self, AppInfo, AppStatus, AppTap, Backend, Follow, Mute, Proc, Stall, Target, STALL,
};
use crate::channels::{AppSinks, ChannelsStatus, Split};
use crate::default_out::{FollowDefault, Now, Outputs};
use crate::watch::{self, Listen, Listened, Wake};
use crate::{Error, ErrorKind, Feed, Permission, Sink};

type OSStatus = i32;

/// The name the private aggregate device carries (visible only to this
/// process).
const AGGREGATE_NAME: &str = "simply-audio-capture tap";

/// A CoreAudio device: what it has, and whether it's the default output.
#[derive(Clone, Debug)]
pub(crate) struct Device {
    pub id: AudioObjectID,
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
        devices.push(Device { id, name, uid, inputs, outputs, is_default_output: id == default });
    }
    Ok(devices)
}

/// The output device `name` exactly, or the default output for `""`.
pub(crate) fn find_output(name: &str) -> Result<Device, Error> {
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
    /// IOProc calls so far: a count that stands still is a stalled tap.
    calls: Arc<AtomicU64>,
    /// How many taps the aggregate holds: its last this many input
    /// streams, summed.
    taps: usize,
}

/// What a tap takes in.
pub(crate) enum Kind {
    /// Every process playing on this output device.
    Device(Device),
    /// The processes the [`Target`]s name (wherever they play, or on one
    /// device), mixed, clocked by `clock` (the default output when the tap
    /// was made).
    Processes { targets: Vec<Target>, mute: Mute, clock: Device },
}

/// A running tap. Dropping it stops the IOProc and destroys the private
/// aggregate device and the tap, in that order.
pub(crate) struct OutputTap {
    taps: Vec<AudioObjectID>,
    aggregate: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    ctx: *mut Ctx,
    /// The device the aggregate is clocked by (the tapped one, for a device
    /// tap): when it goes, the tap goes silent.
    clock: AudioObjectID,
    pub device: String,
    /// The rate the tap delivers (the device's own nominal rate).
    pub sample_rate: u32,
    calls: Arc<AtomicU64>,
    stall: Mutex<Stall>,
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
        OutputTap::open(Kind::Device(find_output(device)?), rate, channels, sink)
            .map_err(|(e, _)| e)
    }

    /// Make the tap `kind` and start it into `sink`. On failure, whatever
    /// was created is undone and the sink comes back.
    pub(crate) fn open(
        kind: Kind,
        rate: u32,
        channels: usize,
        sink: Sink,
    ) -> Result<OutputTap, (Error, Sink)> {
        let clock = match &kind {
            Kind::Device(d) | Kind::Processes { clock: d, .. } => d.clone(),
        };
        let mut t = OutputTap {
            taps: Vec::new(),
            aggregate: 0,
            proc_id: None,
            ctx: std::ptr::null_mut(),
            clock: clock.id,
            device: clock.name.clone(),
            sample_rate: 0,
            calls: Arc::new(AtomicU64::new(0)),
            stall: Mutex::new(Stall::new(Instant::now())),
        };
        let mut slot = Some(sink);
        match unsafe { t.build(&kind, &clock, rate, channels, &mut slot) } {
            Ok(()) => Ok(t),
            Err(e) => {
                // Dropping `t` undoes whatever was created; the sink is
                // either still in the slot or inside the IOProc's context.
                let sink = slot.or_else(|| t.take_sink()).expect("the sink is somewhere");
                Err((e, sink))
            }
        }
    }

    unsafe fn build(
        &mut self,
        kind: &Kind,
        clock: &Device,
        rate: u32,
        channels: usize,
        sink: &mut Option<Sink>,
    ) -> Result<(), Error> {
        let uid = NSString::from_str(&clock.uid);
        let mut tap_uids = Vec::new();
        for desc in descriptions(kind)? {
            tap_uids.push(create_tap(&desc, &mut self.taps)?);
        }
        let refs: Vec<&NSString> = tap_uids.iter().map(|u| &**u).collect();
        self.aggregate = create_aggregate(&uid, &refs)?;
        // The rate the IOProc runs at is the aggregate's, clocked by the
        // tapped device. (The tap's own format property said 48000 for
        // speakers running at 44100 — measured — so it isn't used.)
        let device_rate: f64 = get(
            self.aggregate,
            kAudioDevicePropertyNominalSampleRate,
            kAudioObjectPropertyScopeGlobal,
        )?;
        self.sample_rate = device_rate.round() as u32;

        let mut tb = MachTimebaseInfo { numer: 1, denom: 1 };
        if mach_timebase_info(&mut tb) != 0 || tb.denom == 0 {
            tb = MachTimebaseInfo { numer: 1, denom: 1 };
        }
        let channels = channels.max(1);
        let sink = sink.take().expect("the sink is handed over once");
        self.ctx = Box::into_raw(Box::new(Ctx {
            feed: Feed::new(sink, channels, self.sample_rate, rate),
            channels,
            rate: device_rate,
            scratch: vec![0.0; MAP_FRAMES * channels],
            numer: tb.numer as u64,
            denom: tb.denom as u64,
            calls: Arc::clone(&self.calls),
            taps: refs.len(),
        }));
        check(
            AudioDeviceCreateIOProcID(
                self.aggregate,
                Some(io_proc),
                self.ctx.cast(),
                NonNull::from(&mut self.proc_id),
            ),
            "creating the tap IOProc",
        )?;
        check(AudioDeviceStart(self.aggregate, self.proc_id), "starting the tap")?;
        Ok(())
    }

    /// Stop the IOProc and hand back the sink it fed, if it has it.
    fn take_sink(&mut self) -> Option<Sink> {
        unsafe {
            if self.proc_id.is_some() {
                AudioDeviceStop(self.aggregate, self.proc_id);
                AudioDeviceDestroyIOProcID(self.aggregate, self.proc_id);
                self.proc_id = None;
            }
            if self.ctx.is_null() {
                return None;
            }
            let ctx = Box::from_raw(self.ctx);
            self.ctx = std::ptr::null_mut();
            Some(ctx.feed.into_sink())
        }
    }

    /// Stop and destroy the tap, keeping its sink for the next one.
    pub(crate) fn into_sink(mut self) -> Sink {
        self.take_sink().expect("a running tap holds its sink")
    }

    /// Why this tap no longer delivers, if it doesn't: the device it is
    /// clocked by is gone, or the IOProc stopped being called.
    pub(crate) fn health(&self) -> Option<String> {
        self.health_while(true)
    }

    /// [`health`](OutputTap::health), for a tap whose processes may not be
    /// `playing`: a tap of processes that play nothing is not called back
    /// at all (measured), so then only its clock device is checked.
    pub(crate) fn health_while(&self, playing: bool) -> Option<String> {
        let alive: Result<u32, _> =
            get(self.clock, kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal);
        if !matches!(alive, Ok(1)) {
            return Some(format!("output device {:?} disappeared", self.device));
        }
        let calls = self.calls.load(Ordering::Relaxed);
        let mut stall = self.stall.lock().unwrap_or_else(|e| e.into_inner());
        if !playing {
            stall.rest(calls, Instant::now());
            return None;
        }
        stall.stalled(calls, Instant::now()).then(|| {
            format!("the tap stopped delivering audio (no callback for {} s)", STALL.as_secs())
        })
    }
}

impl Drop for OutputTap {
    fn drop(&mut self) {
        drop(self.take_sink());
        unsafe {
            if self.aggregate != 0 {
                AudioHardwareDestroyAggregateDevice(self.aggregate);
            }
            for &tap in &self.taps {
                AudioHardwareDestroyProcessTap(tap);
            }
        }
    }
}

/// The tap descriptions `kind` needs (private, with its mute behaviour):
/// one per target, and one per output stream of the device a
/// [`Target::OnDevice`] names.
///
/// Every tap is a stereo mixdown except a [`Target::OnDevice`] one: a
/// mixdown tap of processes on one device stream is not scoped to it at
/// all, it hears what they play everywhere (measured on macOS 26.6 by
/// `two_devices_a_scoped_tap_and_the_devices_listener`). Left alone, it
/// takes the stream's own format, which the IOProc maps to the width asked
/// for like a device's input (its first channels).
unsafe fn descriptions(kind: &Kind) -> Result<Vec<Retained<CATapDescription>>, Error> {
    let ids = |objects: &[u32]| {
        let n: Vec<_> = objects.iter().map(|o| NSNumber::new_u32(*o)).collect();
        NSArray::from_retained_slice(&n)
    };
    let alloc = CATapDescription::alloc;
    let mut descs: Vec<(Retained<CATapDescription>, bool)> = Vec::new(); // (desc, mixdown)
    let mute = match kind {
        Kind::Device(d) => {
            let uid = NSString::from_str(&d.uid);
            let none = NSArray::<NSNumber>::new();
            descs.push((
                CATapDescription::initExcludingProcesses_andDeviceUID_withStream(
                    alloc(),
                    &none,
                    &uid,
                    0,
                ),
                true,
            ));
            Mute::Unmuted
        }
        Kind::Processes { targets, mute, .. } => {
            for target in targets {
                match target {
                    Target::Processes(o) => descs.push((
                        CATapDescription::initStereoMixdownOfProcesses(alloc(), &ids(o)),
                        true,
                    )),
                    Target::AllExcept(o) => descs.push((
                        CATapDescription::initStereoGlobalTapButExcludeProcesses(alloc(), &ids(o)),
                        true,
                    )),
                    Target::OnDevice(o, uid) => {
                        let uid_ns = NSString::from_str(uid);
                        for stream in 0..output_streams(uid)? {
                            let desc = CATapDescription::initWithProcesses_andDeviceUID_withStream(
                                alloc(),
                                &ids(o),
                                &uid_ns,
                                stream as _,
                            );
                            descs.push((desc, false));
                        }
                    }
                }
            }
            *mute
        }
    };
    let mute = match mute {
        Mute::Unmuted => CATapMuteBehavior::Unmuted,
        Mute::Muted => CATapMuteBehavior::Muted,
        Mute::MutedWhenTapped => CATapMuteBehavior::MutedWhenTapped,
    };
    let name = NSString::from_str(AGGREGATE_NAME);
    Ok(descs
        .into_iter()
        .map(|(desc, mixdown)| {
            desc.setName(&name);
            if mixdown {
                desc.setMixdown(true);
                desc.setMono(false);
            }
            desc.setPrivate(true);
            desc.setMuteBehavior(mute);
            desc
        })
        .collect())
}

/// How many output streams the device with this UID has.
fn output_streams(uid: &str) -> Result<usize, Error> {
    let device = list_devices()?
        .into_iter()
        .find(|d| d.uid == uid && d.outputs)
        .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("output device {uid:?} is gone")))?;
    Ok(get_vec::<AudioObjectID>(
        device.id,
        kAudioDevicePropertyStreams,
        kAudioObjectPropertyScopeOutput,
    )?
    .len())
}

/// Make the process tap `desc` describes, push it onto `taps`, and return
/// its UID. It is pushed before anything can fail after it, so the caller
/// destroys it either way.
unsafe fn create_tap(
    desc: &CATapDescription,
    taps: &mut Vec<AudioObjectID>,
) -> Result<Retained<NSString>, Error> {
    let mut tap = 0;
    check(
        AudioHardwareCreateProcessTap(Some(desc), &mut tap),
        "creating the process tap (macOS 14.2+)",
    )?;
    taps.push(tap);
    Ok(desc.UUID().UUIDString())
}

fn key(k: &CStr) -> Retained<NSString> {
    NSString::from_str(k.to_str().unwrap_or_default())
}

/// A private aggregate device clocked by (and holding) the device `clock`,
/// with the taps `taps` (by UID) as its inputs, started automatically.
unsafe fn create_aggregate(clock: &NSString, taps: &[&NSString]) -> Result<AudioObjectID, Error> {
    let sub_device = NSDictionary::<NSString, AnyObject>::from_slices(
        &[&*key(kAudioSubDeviceUIDKey)],
        &[clock as &AnyObject],
    );
    let yes = NSNumber::new_bool(true);
    let sub_taps: Vec<_> = taps
        .iter()
        .map(|uid| {
            NSDictionary::<NSString, AnyObject>::from_slices(
                &[&*key(kAudioSubTapUIDKey), &*key(kAudioSubTapDriftCompensationKey)],
                &[*uid as &AnyObject, &*yes],
            )
        })
        .collect();
    let agg_uid =
        NSString::from_str(&format!("simply-audio-capture-tap-{}", NSUUID::new().UUIDString()));
    let agg_name = NSString::from_str(AGGREGATE_NAME);
    let subs = NSArray::from_slice(&[&*sub_device]);
    let taps = NSArray::from_retained_slice(&sub_taps);
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
        &[&*agg_uid as &AnyObject, &*agg_name, clock, &*yes, &*yes, &*subs, &*taps],
    );
    // NSDictionary is toll-free bridged to CFDictionary.
    let cf: &CFDictionary = &*(Retained::as_ptr(&agg) as *const CFDictionary);
    let mut aggregate = 0;
    check(
        AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut aggregate)),
        "creating the private aggregate device",
    )?;
    Ok(aggregate)
}

/// The real-time callback: the taps' frames, each mapped to `channels`
/// wide and summed, into the feed. No allocation, locks, logging or I/O.
/// The taps are the aggregate's last input streams (a tapped device's own
/// inputs, if it has any, come first).
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
    ctx.calls.fetch_add(1, Ordering::Relaxed);
    let list = input.as_ptr();
    let n = (*list).mNumberBuffers as usize;
    if n == 0 {
        return 0;
    }
    let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), n);
    let taps = &buffers[n - ctx.taps.clamp(1, n)..];
    let samples = |buf: &AudioBuffer| -> (&[f32], usize) {
        if buf.mData.is_null() {
            return (&[], 1);
        }
        let ch = buf.mNumberChannels.max(1) as usize;
        (std::slice::from_raw_parts(buf.mData as *const f32, buf.mDataByteSize as usize / 4), ch)
    };
    let time = input_time.as_ref();
    let ns = if time.mFlags.contains(AudioTimeStampFlags::HostTimeValid) {
        (time.mHostTime as u128 * ctx.numer as u128 / ctx.denom as u128) as u64
    } else {
        0
    };
    let ch = ctx.channels;
    if let [one] = taps {
        let (s, src_ch) = samples(one);
        if src_ch == ch {
            let frames = s.len() / src_ch;
            ctx.feed.push(&s[..frames * src_ch], ns);
            return 0;
        }
    }
    // Every tap of one aggregate is called with the same frame count.
    let frames = taps.iter().map(samples).map(|(s, c)| s.len() / c).max().unwrap_or(0);
    let mut done = 0;
    while done < frames {
        let count = (frames - done).min(MAP_FRAMES);
        let out = &mut ctx.scratch[..count * ch];
        out.fill(0.0);
        for buf in taps {
            let (s, src_ch) = samples(buf);
            let have = (s.len() / src_ch).saturating_sub(done).min(count);
            for f in 0..have {
                let frame = &s[(done + f) * src_ch..(done + f + 1) * src_ch];
                for c in 0..ch {
                    out[f * ch + c] += frame[c.min(src_ch - 1)];
                }
            }
        }
        let at = if ns == 0 { 0 } else { ns + (done as f64 * 1e9 / ctx.rate) as u64 };
        ctx.feed.push(&ctx.scratch[..count * ch], at);
        done += count;
    }
    0
}

/// Identity of a process object, read once: the pid, bundle id and path
/// don't change for an object's life.
type Identity = (i32, Option<String>, Option<String>);

/// Every process object the HAL lists, with who it is and whether it is
/// playing. `cache` keeps identities across calls (pruned to the objects
/// still listed), so an idle poll costs one property read per process.
/// With `devices`, also the UIDs of the output devices each one plays into
/// (a few more reads per process).
fn list_processes(cache: &mut HashMap<AudioObjectID, Identity>, devices: bool) -> Vec<Proc> {
    let objects: Vec<AudioObjectID> = get_vec(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyProcessObjectList,
        kAudioObjectPropertyScopeGlobal,
    )
    .unwrap_or_default();
    cache.retain(|o, _| objects.contains(o));
    let mut procs = Vec::with_capacity(objects.len());
    for object in objects {
        let identity = match cache.get(&object) {
            Some(i) => i.clone(),
            None => {
                let Ok(pid) =
                    get::<i32>(object, kAudioProcessPropertyPID, kAudioObjectPropertyScopeGlobal)
                else {
                    continue; // on its way out
                };
                let bundle = get_string(object, kAudioProcessPropertyBundleID)
                    .ok()
                    .filter(|b| !b.is_empty());
                let i = (pid, bundle, pid_path(pid));
                cache.insert(object, i.clone());
                i
            }
        };
        let playing = get::<u32>(
            object,
            kAudioProcessPropertyIsRunningOutput,
            kAudioObjectPropertyScopeGlobal,
        )
        .is_ok_and(|v| v == 1);
        let (pid, bundle, exe) = identity;
        let devices = match devices {
            true => output_devices(object)
                .into_iter()
                .filter_map(|d| get_string(d, kAudioDevicePropertyDeviceUID).ok())
                .collect(),
            false => Vec::new(),
        };
        procs.push(Proc { object, pid, bundle, exe, playing, devices });
    }
    procs
}

/// The output devices a process object plays to.
fn output_devices(object: AudioObjectID) -> Vec<AudioObjectID> {
    get_vec(object, kAudioProcessPropertyDevices, kAudioObjectPropertyScopeOutput)
        .unwrap_or_default()
}

/// Whether a process object plays to any output device.
fn has_output(object: AudioObjectID) -> bool {
    !output_devices(object).is_empty()
}

/// The audio clients that play (or can play) audio: what a picker lists.
pub(crate) fn list_apps() -> Vec<AppInfo> {
    let procs: Vec<Proc> = list_processes(&mut HashMap::new(), false)
        .into_iter()
        .filter(|p| p.playing || has_output(p.object))
        .collect();
    apps::infos(&procs, std::process::id() as i32)
}

/// `kAudioHardwarePropertyTranslatePIDToProcessObject`: `pid`'s process
/// object, or None when it isn't an audio client.
fn translate_pid(pid: i32) -> Option<AudioObjectID> {
    let addr =
        address(kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyScopeGlobal);
    let mut object: AudioObjectID = 0;
    let mut size = std::mem::size_of::<AudioObjectID>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&addr),
            std::mem::size_of::<i32>() as u32,
            (&pid as *const i32).cast(),
            NonNull::from(&mut size),
            NonNull::from(&mut object).cast(),
        )
    };
    (status == 0 && object != kAudioObjectUnknown).then_some(object)
}

/// The executable's path (`proc_pidpath`), if the process is still there.
fn pid_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// The real [`Backend`]: process taps clocked by the default output.
struct Hal {
    rate: u32,
    channels: usize,
    cache: HashMap<AudioObjectID, Identity>,
    /// Read the devices each process plays into (the tap lists an
    /// [`App::PlayingInto`](crate::App::PlayingInto)).
    devices: bool,
}

impl Backend for Hal {
    type Tap = OutputTap;

    fn processes(&mut self) -> Vec<Proc> {
        list_processes(&mut self.cache, self.devices)
    }

    fn translate(&mut self, pid: i32) -> Option<u32> {
        translate_pid(pid)
    }

    fn open(
        &mut self,
        targets: &[Target],
        mute: Mute,
        sink: Sink,
    ) -> Result<(OutputTap, u32), (Error, Sink)> {
        let clock = match find_output("") {
            Ok(d) => d,
            Err(e) => return Err((e, sink)),
        };
        let kind = Kind::Processes { targets: targets.to_vec(), mute, clock };
        let tap = OutputTap::open(kind, self.rate, self.channels, sink)?;
        let rate = tap.sample_rate;
        Ok((tap, rate))
    }

    fn close(&mut self, tap: OutputTap) -> Sink {
        tap.into_sink()
    }

    fn health(&mut self, tap: &OutputTap, playing: bool) -> Option<String> {
        tap.health_while(playing)
    }
}

/// How often a follower looks even when the HAL said nothing (it does say
/// when a process connects or goes; this is the backstop, and the cadence
/// of the health check).
const POLL: Duration = Duration::from_millis(500);

unsafe extern "C-unwind" fn on_change(
    _object: AudioObjectID,
    _count: u32,
    _addresses: NonNull<AudioObjectPropertyAddress>,
    client: *mut c_void,
) -> OSStatus {
    (*(client as *const Wake)).poke(false);
    0
}

/// The pointer HAL listeners poke `wake` through. A listener may still be
/// running on a HAL thread as it is removed, so the pointer keeps its own
/// reference, never released: one small allocation per capture, never
/// touched once freed.
fn client_of(wake: &Arc<Wake>) -> *const Wake {
    Arc::into_raw(Arc::clone(wake))
}

/// Add or remove [`on_change`] for `client` on `object`'s `selector`.
fn listener(
    add: bool,
    object: AudioObjectID,
    (selector, scope): (AudioObjectPropertySelector, AudioObjectPropertyScope),
    client: *const Wake,
) {
    let addr = address(selector, scope);
    unsafe {
        let f =
            if add { AudioObjectAddPropertyListener } else { AudioObjectRemovePropertyListener };
        // A process that is already gone refuses the removal: nothing to do.
        f(object, NonNull::from(&addr), Some(on_change), client as *mut c_void);
    }
}

/// A thread that ticks a follower: once at the start, then whenever one of
/// its system-object HAL properties changes (or [`POLL`] passes), until it
/// is dropped.
struct Watcher {
    wake: Arc<Wake>,
    /// The pointer the listeners were registered with.
    client: *const Wake,
    selectors: &'static [AudioObjectPropertySelector],
    thread: Option<JoinHandle<()>>,
}

// `client` is only handed to CoreAudio and compared on removal.
unsafe impl Send for Watcher {}
unsafe impl Sync for Watcher {}

impl Watcher {
    /// Run `tick` on a thread called `name`, woken by `wake` (see
    /// [`watch::spawn`]).
    fn start<T>(
        name: &str,
        wake: Arc<Wake>,
        selectors: &'static [AudioObjectPropertySelector],
        tick: T,
    ) -> Result<Watcher, Error>
    where
        T: FnMut(Instant) -> Result<(), Error> + Send + 'static,
    {
        let thread = watch::spawn(name, Arc::clone(&wake), tick)?;
        let client = client_of(&wake);
        for &selector in selectors {
            let at = (selector, kAudioObjectPropertyScopeGlobal);
            listener(true, kAudioObjectSystemObject as AudioObjectID, at, client);
        }
        Ok(Watcher { wake, client, selectors, thread: Some(thread) })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        for &selector in self.selectors {
            let at = (selector, kAudioObjectPropertyScopeGlobal);
            listener(false, kAudioObjectSystemObject as AudioObjectID, at, self.client);
        }
        self.wake.poke(true);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The HAL's [`Listen`]: wakes the follower when a process object's
/// `kAudioProcessPropertyDevices` changes (it moved to another device).
struct DevicesListen {
    client: *const Wake,
}

// `client` is only handed to CoreAudio.
unsafe impl Send for DevicesListen {}

/// A process's devices, in any scope (output is the one read).
const DEVICES: (AudioObjectPropertySelector, AudioObjectPropertyScope) =
    (kAudioProcessPropertyDevices, kAudioObjectPropertyScopeWildcard);

impl Listen for DevicesListen {
    fn add(&mut self, object: u32) {
        listener(true, object, DEVICES, self.client);
    }
    fn remove(&mut self, object: u32) {
        listener(false, object, DEVICES, self.client);
    }
}

/// What pokes an app follower: a process came or went, a device list
/// changed. (And, for a tap that reads them, a process's devices changed:
/// [`DevicesListen`].)
const APPS_WATCHED: [AudioObjectPropertySelector; 2] =
    [kAudioHardwarePropertyProcessObjectList, kAudioHardwarePropertyDevices];

/// The backend an app follower runs on: the HAL, with a listener on each
/// process's devices when the tap reads them.
fn app_backend(
    tap: &AppTap,
    rate: u32,
    channels: usize,
    wake: &Arc<Wake>,
) -> Listened<Hal, DevicesListen> {
    let hal = Hal { rate, channels, cache: HashMap::new(), devices: tap.needs_devices() };
    Listened::new(hal, tap, || DevicesListen { client: client_of(wake) })
}

/// A running app follower: its thread owns the [`Follow`] and the tap.
pub(crate) struct Following {
    status: Arc<Mutex<AppStatus>>,
    _watcher: Watcher,
}

impl Following {
    /// Start following `tap` into `sink`. Returns after the first look:
    /// tapping, waiting for the app, or the error that stopped the first
    /// tap being made.
    pub(crate) fn start(
        tap: AppTap,
        rate: u32,
        channels: usize,
        sink: Sink,
    ) -> Result<Following, Error> {
        Following::start_polling(tap, rate, channels, sink, POLL)
    }

    /// [`start`](Following::start), looking every `poll` when nothing woke
    /// it (a hand check makes it long, to see the listeners at work).
    fn start_polling(
        tap: AppTap,
        rate: u32,
        channels: usize,
        sink: Sink,
        poll: Duration,
    ) -> Result<Following, Error> {
        let status = Arc::new(Mutex::new(AppStatus::Failed { error: "starting".into() }));
        let wake = Arc::new(Wake::new(poll));
        let backend = app_backend(&tap, rate, channels, &wake);
        let mut follow = Follow::new(backend, tap, std::process::id() as i32, sink);
        let shared = Arc::clone(&status);
        let watcher = Watcher::start("audio app tap", wake, &APPS_WATCHED, move |now| {
            let s = follow.tick(now).clone();
            *shared.lock().unwrap_or_else(|e| e.into_inner()) = s.clone();
            match s {
                AppStatus::Failed { error } => Err(Error::new(ErrorKind::Device, error)),
                _ => Ok(()),
            }
        })?;
        Ok(Following { status, _watcher: watcher })
    }

    pub(crate) fn status(&self) -> AppStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// A running per-app follower: its thread owns the [`Split`], its taps and
/// the caller's [`AppSinks`].
pub(crate) struct Channels {
    status: Arc<Mutex<ChannelsStatus>>,
    _watcher: Watcher,
}

impl Channels {
    /// Start following `tap`, one channel per app, into `sinks`. Returns
    /// after the first look (the apps playing now have their channels).
    pub(crate) fn start<S: AppSinks>(
        tap: AppTap,
        rate: u32,
        channels: usize,
        sinks: S,
    ) -> Result<Channels, Error> {
        let status = Arc::new(Mutex::new(ChannelsStatus::default()));
        let wake = Arc::new(Wake::new(POLL));
        let backend = app_backend(&tap, rate, channels, &wake);
        let mut split = Split::new(backend, tap, std::process::id() as i32, sinks);
        let shared = Arc::clone(&status);
        let watcher = Watcher::start("audio app channels", wake, &APPS_WATCHED, move |now| {
            let s = split.tick(now).clone();
            *shared.lock().unwrap_or_else(|e| e.into_inner()) = s;
            Ok(())
        })?;
        Ok(Channels { status, _watcher: watcher })
    }

    pub(crate) fn status(&self) -> ChannelsStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The real [`Outputs`]: device taps on whatever the default output is.
struct Defaults {
    rate: u32,
    channels: usize,
}

impl Outputs for Defaults {
    type Tap = OutputTap;

    fn default_output(&mut self) -> Option<u32> {
        get::<AudioObjectID>(
            kAudioObjectSystemObject as AudioObjectID,
            kAudioHardwarePropertyDefaultOutputDevice,
            kAudioObjectPropertyScopeGlobal,
        )
        .ok()
        .filter(|id| *id != kAudioObjectUnknown)
    }

    fn open(&mut self, id: u32, sink: Sink) -> Result<(OutputTap, String, u32), (Error, Sink)> {
        let found = list_devices().map(|d| d.into_iter().find(|d| d.id == id && d.outputs));
        let device = match found {
            Ok(Some(d)) => d,
            Ok(None) => {
                let e = Error::new(ErrorKind::NotFound, "the default output device went away");
                return Err((e, sink));
            }
            Err(e) => return Err((e, sink)),
        };
        let tap = OutputTap::open(Kind::Device(device), self.rate, self.channels, sink)?;
        let (name, rate) = (tap.device.clone(), tap.sample_rate);
        Ok((tap, name, rate))
    }

    fn close(&mut self, tap: OutputTap) -> Sink {
        tap.into_sink()
    }

    fn health(&mut self, tap: &OutputTap) -> Option<String> {
        tap.health()
    }
}

/// What pokes a default-output follower: the default output changed.
const DEFAULT_WATCHED: [AudioObjectPropertySelector; 1] =
    [kAudioHardwarePropertyDefaultOutputDevice];

/// A running tap of the default output that follows it to a new device:
/// its thread owns the [`FollowDefault`] and the tap.
pub(crate) struct DefaultTap {
    now: Arc<Mutex<Now>>,
    _watcher: Watcher,
}

impl DefaultTap {
    /// Tap the default output into `sink`. Returns once audio is flowing,
    /// or with why not.
    pub(crate) fn start(rate: u32, channels: usize, sink: Sink) -> Result<DefaultTap, Error> {
        let now = Arc::new(Mutex::new(Now::default()));
        let mut follow = FollowDefault::new(Defaults { rate, channels }, sink);
        let shared = Arc::clone(&now);
        let wake = Arc::new(Wake::new(POLL));
        let watcher =
            Watcher::start("audio default output tap", wake, &DEFAULT_WATCHED, move |t| {
                let r = follow.tick(t).map(|_| ());
                *shared.lock().unwrap_or_else(|e| e.into_inner()) = follow.now().clone();
                r
            })?;
        Ok(DefaultTap { now, _watcher: watcher })
    }

    pub(crate) fn now(&self) -> Now {
        self.now.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
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

    /// Reading the process list needs no permission and opens nothing:
    /// coreaudiod always has clients, each with a pid, and translating a
    /// listed pid gives its object back.
    #[test]
    fn the_hal_lists_audio_processes_and_translates_their_pids() {
        let procs = list_processes(&mut HashMap::new(), true);
        assert!(!procs.is_empty(), "the HAL should always have client processes");
        for p in &procs {
            assert!(p.pid > 0, "process object {} reported pid {}", p.object, p.pid);
            assert!(p.devices.iter().all(|uid| !uid.is_empty()), "{p:?}");
        }
        let p = &procs[0];
        assert_eq!(translate_pid(p.pid), Some(p.object));
        assert_eq!(translate_pid(-1), None);
        assert!(list_apps().iter().all(|a| !a.name.is_empty()));
    }

    /// Follows a real app tap of everything except this process, then
    /// stops it: the private aggregate must be gone. Ignored for the same
    /// reason as the next one.
    /// The two real-tap tests count aggregates by name, so they must not
    /// overlap: cargo runs tests in parallel by default.
    static REAL_TAP: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    #[ignore]
    fn an_app_tap_leaves_no_aggregate_device_behind() {
        let _serial = REAL_TAP.lock().unwrap_or_else(|e| e.into_inner());
        let ours = || list_devices().unwrap().iter().filter(|o| o.name == AGGREGATE_NAME).count();
        let tap = AppTap::all_except([crate::App::This]);
        let f = Following::start(tap, 48_000, 2, Box::new(|_: &[f32], _: u64| {})).unwrap();
        eprintln!("{}", f.status());
        assert!(f.status().is_tapping());
        assert_eq!(ours(), 1);
        drop(f);
        assert_eq!(ours(), 0);
    }

    /// An app tap of this test process while it is an audio client that
    /// plays nothing: its tap gets no callbacks, and must be kept rather
    /// than re-made every [`STALL`] as a stalled one would be. The
    /// aggregate's object id staying the same is what shows it was not.
    /// Ignored for the same reason as the others.
    #[test]
    #[ignore]
    fn a_paused_app_s_tap_is_kept_not_re_made() {
        let _serial = REAL_TAP.lock().unwrap_or_else(|e| e.into_inner());
        let ours = || -> Vec<AudioObjectID> {
            list_devices()
                .unwrap()
                .iter()
                .filter(|o| o.name == AGGREGATE_NAME)
                .map(|o| o.id)
                .collect()
        };
        let stream = silence(); // a client, never started
        let tap = AppTap::only([crate::App::This]);
        let f = Following::start(tap, 48_000, 2, Box::new(|_: &[f32], _: u64| {})).unwrap();
        eprintln!("{}", f.status());
        assert!(f.status().is_tapping());
        let first = ours();
        assert_eq!(first.len(), 1);
        std::thread::sleep(STALL * 2 + POLL * 2);
        assert!(f.status().is_tapping(), "{}", f.status());
        assert_eq!(ours(), first, "the paused app's tap was re-made");
        drop(f);
        assert!(ours().is_empty());
        drop(stream);
    }

    /// A process playing (silence) out of this test, tapped on a channel of
    /// its own: the channel starts, its sink gets frames, and dropping the
    /// capture stops it and leaves no aggregate behind. Ignored for the
    /// same reason as the others.
    #[test]
    #[ignore]
    fn an_app_channel_starts_delivers_and_leaves_no_aggregate_behind() {
        use crate::channels::{AppSinks, ChannelState};
        let _serial = REAL_TAP.lock().unwrap_or_else(|e| e.into_inner());
        let ours = || list_devices().unwrap().iter().filter(|o| o.name == AGGREGATE_NAME).count();
        let stream = silence();
        cpal::traits::StreamTrait::play(&stream).unwrap();

        #[derive(Clone, Default)]
        struct Heard(Arc<Mutex<Vec<String>>>, Arc<AtomicU64>);
        impl AppSinks for Heard {
            type Sink = Box<dyn FnMut(&[f32], u64) + Send>;
            fn started(&mut self, app: &AppInfo) -> Self::Sink {
                self.0.lock().unwrap().push(format!("+{}", app.pid));
                let frames = Arc::clone(&self.1);
                Box::new(move |f: &[f32], _| {
                    frames.fetch_add(f.len() as u64 / 2, Ordering::Relaxed);
                })
            }
            fn stopped(&mut self, app: &AppInfo) {
                self.0.lock().unwrap().push(format!("-{}", app.pid));
            }
        }
        let heard = Heard::default();
        let tap = AppTap::only([crate::App::This]);
        let c = Channels::start(tap, 48_000, 2, heard.clone()).unwrap();
        let me = std::process::id();
        std::thread::sleep(Duration::from_secs(1));
        let status = c.status();
        eprintln!("{status:?}");
        assert_eq!(status.channels.len(), 1);
        assert!(matches!(status.channels[0].state, ChannelState::Tapping { .. }));
        assert_eq!(ours(), 1);
        assert!(heard.1.load(Ordering::Relaxed) > 0, "the channel's sink got no frames");
        drop(c);
        assert_eq!(*heard.0.lock().unwrap(), [format!("+{me}"), format!("-{me}")]);
        assert_eq!(ours(), 0);
        drop(stream);
    }

    /// The hand check for a process playing into two devices at once, and
    /// for the follower being woken when a process changes device. Needs a
    /// second output device (BlackHole, a Tussen device, any other one),
    /// and skips itself unless System Audio Recording is granted, so it
    /// never raises the prompt:
    /// `cargo test -p simply-audio-capture two_devices -- --ignored --nocapture`.
    ///
    /// This process plays a 997 Hz tone at -80 dBFS (inaudible) to the
    /// default output and a 1499 Hz tone at -12 dBFS to the other device.
    /// 1. Device-scoped taps of this process (`CATapDescription
    ///    initWithProcesses:andDeviceUID:withStream:`): the one on the other
    ///    device hears only 1499 Hz, the one on the default output only
    ///    997 Hz; a plain process tap hears both. (Made a stereo mixdown, a
    ///    scoped tap hears both too: see [`descriptions`].)
    /// 2. "Everything except whatever plays into the other device", with a
    ///    60 s poll: the other device's stream is made only after the
    ///    follower started, so only the listener on this process's devices
    ///    can make the follower re-make its taps within the 5 s allowed.
    ///    Then it hears 997 Hz (this process's default-output part) and not
    ///    1499 Hz. Other apps playing on the Mac add to what it hears.
    #[test]
    #[ignore]
    fn two_devices_a_scoped_tap_and_the_devices_listener() {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let _serial = REAL_TAP.lock().unwrap_or_else(|e| e.into_inner());
        if permission() != Permission::Granted {
            eprintln!("skipped: System Audio Recording is {:?}", permission());
            return;
        }
        let default = find_output("").unwrap();
        let outputs: Vec<Device> = list_devices()
            .unwrap()
            .into_iter()
            .filter(|d| d.outputs && d.id != default.id)
            .collect();
        let Some(other) = outputs
            .iter()
            .find(|d| d.name.contains("BlackHole") || d.uid.starts_with("com.simply.audio.tussen."))
            .or(outputs.iter().find(|d| d.name != AGGREGATE_NAME))
            .cloned()
        else {
            eprintln!("skipped: no second output device");
            return;
        };
        eprintln!(
            "default output {:?}, other device {:?} ({})",
            default.name, other.name, other.uid
        );
        let tone = |name: &str, hz: f32, amp: f32| -> cpal::Stream {
            let host = cpal::default_host();
            let dev = host
                .output_devices()
                .unwrap()
                .find(|d| crate::device_name(d).as_deref() == Some(name))
                .unwrap_or_else(|| panic!("cpal has no {name:?}"));
            let config = dev.default_output_config().unwrap().config();
            let (rate, ch) = (config.sample_rate as f32, config.channels as usize);
            let mut phase = 0f32;
            let s = dev
                .build_output_stream::<f32, _, _>(
                    config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        for frame in data.chunks_mut(ch) {
                            frame.fill(amp * (phase * std::f32::consts::TAU).sin());
                            phase = (phase + hz / rate).fract();
                        }
                    },
                    |_| {},
                    None,
                )
                .unwrap();
            s.play().unwrap();
            s
        };
        type Heard = Arc<Mutex<Vec<f32>>>;
        let into = |heard: &Heard| -> Sink {
            let heard = Arc::clone(heard);
            Box::new(move |f: &[f32], _| {
                heard.lock().unwrap().extend(f.chunks(2).map(|c| c[0]));
            })
        };
        // Amplitude of `hz` in the last second heard (Goertzel).
        let level = |heard: &Heard, hz: f64| -> f64 {
            let h = heard.lock().unwrap();
            let x = &h[h.len().saturating_sub(48_000)..];
            let k = 2.0 * (std::f64::consts::TAU * hz / 48_000.0).cos();
            let (mut s1, mut s2) = (0.0, 0.0);
            for &v in x {
                let s0 = v as f64 + k * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            let power = s1 * s1 + s2 * s2 - k * s1 * s2;
            2.0 * power.max(0.0).sqrt() / x.len().max(1) as f64
        };
        let (quiet, loud) = (1e-4f64, 0.25f64);

        let _near = tone(&default.name, 997.0, quiet as f32);
        let far = tone(&other.name, 1499.0, loud as f32);
        std::thread::sleep(Duration::from_millis(500));
        let me = translate_pid(std::process::id() as i32).expect("playing makes a client");
        let mine = list_processes(&mut HashMap::new(), true).into_iter().find(|p| p.object == me);
        eprintln!("this process plays into {:?}", mine.map(|p| p.devices));

        // 1. The API itself.
        let tap_for = |targets: Vec<Target>| {
            let heard = Heard::default();
            let kind = Kind::Processes { targets, mute: Mute::Unmuted, clock: default.clone() };
            let tap = OutputTap::open(kind, 48_000, 2, into(&heard)).map_err(|(e, _)| e).unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            let got = (level(&heard, 997.0), level(&heard, 1499.0));
            drop(tap);
            got
        };
        let on_other = tap_for(vec![Target::OnDevice(vec![me], other.uid.clone())]);
        let on_default = tap_for(vec![Target::OnDevice(vec![me], default.uid.clone())]);
        let whole = tap_for(vec![Target::Processes(vec![me])]);
        eprintln!(
            "scoped to {:?}: 997 Hz {:.2e}, 1499 Hz {:.2e}",
            other.name, on_other.0, on_other.1
        );
        eprintln!(
            "scoped to {:?}: 997 Hz {:.2e}, 1499 Hz {:.2e}",
            default.name, on_default.0, on_default.1
        );
        eprintln!("whole process: 997 Hz {:.2e}, 1499 Hz {:.2e}", whole.0, whole.1);
        assert!(on_other.1 > loud / 2.0 && on_other.0 < quiet / 10.0, "{on_other:?}");
        assert!(on_default.0 > quiet / 2.0 && on_default.1 < quiet, "{on_default:?}");
        assert!(whole.0 > quiet / 2.0 && whole.1 > loud / 2.0, "{whole:?}");

        // 2. The follower, woken by this process's devices changing.
        drop(far);
        std::thread::sleep(Duration::from_millis(500));
        let ours = || -> Vec<AudioObjectID> {
            list_devices()
                .unwrap()
                .iter()
                .filter(|d| d.name == AGGREGATE_NAME)
                .map(|d| d.id)
                .collect()
        };
        let heard = Heard::default();
        let tap = AppTap::all_except([crate::App::PlayingInto(other.uid.clone())]);
        let f = Following::start_polling(tap, 48_000, 2, into(&heard), Duration::from_secs(60))
            .unwrap();
        assert!(f.status().is_tapping(), "{}", f.status());
        let first = ours();
        let _far = tone(&other.name, 1499.0, loud as f32);
        let t = Instant::now();
        while ours() == first {
            assert!(t.elapsed() < Duration::from_secs(5), "not woken by the device change");
            std::thread::sleep(Duration::from_millis(5));
        }
        eprintln!(
            "re-made {:.0} ms after the other device's stream started",
            t.elapsed().as_secs_f64() * 1e3
        );
        std::thread::sleep(Duration::from_millis(1500));
        let got = (level(&heard, 997.0), level(&heard, 1499.0));
        eprintln!(
            "everything except {:?}: 997 Hz {:.2e}, 1499 Hz {:.2e}; {}",
            other.name,
            got.0,
            got.1,
            f.status()
        );
        assert!(got.0 > quiet / 2.0 && got.1 < loud / 100.0, "{got:?}");
        drop(f);
        assert!(ours().is_empty());
    }

    /// Make this process an audio client of the default output, with a
    /// stream of silence that plays once started.
    fn silence() -> cpal::Stream {
        use cpal::traits::{DeviceTrait, HostTrait};
        let out = cpal::default_host().default_output_device().unwrap();
        let config = out.default_output_config().unwrap();
        out.build_output_stream::<f32, _, _>(
            config.config(),
            |data: &mut [f32], _: &cpal::OutputCallbackInfo| data.fill(0.0),
            |_| {},
            None,
        )
        .unwrap()
    }

    /// What the per-app capture's choice rests on, measured on this Mac:
    /// what a one-process tap costs to make and to destroy, how long until
    /// its first callback, and whether a tap can join or leave an aggregate
    /// that is already running (and what that does to the audio already
    /// flowing through it). Prints its findings; asserts only that it
    /// cleaned up. Ignored like the others; it skips itself while System
    /// Audio Recording was never asked for, so it never raises the prompt:
    /// `cargo test -p simply-audio-capture measure_tap_costs -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn measure_tap_costs() {
        let _serial = REAL_TAP.lock().unwrap_or_else(|e| e.into_inner());
        let ours = || list_devices().unwrap().iter().filter(|o| o.name == AGGREGATE_NAME).count();
        let perm = permission();
        eprintln!("permission: {perm:?}");
        if perm == Permission::Unknown {
            eprintln!("skipped: the first tap would raise the permission prompt");
            return;
        }
        let clock = find_output("").unwrap();
        let one = |object: u32| Kind::Processes {
            targets: vec![Target::Processes(vec![object])],
            mute: Mute::Unmuted,
            clock: clock.clone(),
        };
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let first_call = |tap: &OutputTap, t0: Instant| {
            while tap.calls.load(Ordering::Relaxed) == 0 && t0.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_micros(200));
            }
            match tap.calls.load(Ordering::Relaxed) {
                0 => "none in 2 s".to_owned(),
                _ => format!("{:.1} ms", ms(t0.elapsed())),
            }
        };
        let quiet = || Box::new(|_: &[f32], _: u64| {}) as Sink;

        // A process that is not playing: does its tap call back at all?
        let procs = list_processes(&mut HashMap::new(), false);
        for p in procs.iter().filter(|p| !p.playing).take(2) {
            let t0 = Instant::now();
            let tap =
                OutputTap::open(one(p.object), 48_000, 2, quiet()).map_err(|(e, _)| e).unwrap();
            eprintln!(
                "tap of pid {:>6} (not playing): open {:.1} ms, first callback {}",
                p.pid,
                ms(t0.elapsed()),
                first_call(&tap, t0)
            );
        }

        // This process, made an audio client but silent, then made to play
        // (silence) while its tap is open: when does the tap start?
        let stream = silence();
        let me = translate_pid(std::process::id() as i32).expect("an output stream makes a client");
        let t0 = Instant::now();
        let tap = OutputTap::open(one(me), 48_000, 2, quiet()).map_err(|(e, _)| e).unwrap();
        let opened = t0.elapsed();
        std::thread::sleep(Duration::from_millis(300));
        let before = tap.calls.load(Ordering::Relaxed);
        let t1 = Instant::now();
        cpal::traits::StreamTrait::play(&stream).unwrap();
        eprintln!(
            "tap of this process: open {:.1} ms, {before} callbacks while silent, first callback {} after it starts playing",
            ms(opened),
            first_call(&tap, t1)
        );
        drop(tap);

        // A playing process: make, first callback, destroy, five times.
        for _ in 0..5 {
            let t0 = Instant::now();
            let tap = OutputTap::open(one(me), 48_000, 2, quiet()).map_err(|(e, _)| e).unwrap();
            let opened = t0.elapsed();
            let first = first_call(&tap, t0);
            let t1 = Instant::now();
            drop(tap);
            eprintln!(
                "tap of this process (playing): open {:6.1} ms, first callback {first}, close {:6.1} ms",
                ms(opened),
                ms(t1.elapsed())
            );
        }
        assert_eq!(ours(), 0);

        // One aggregate, taps added and removed while it runs.
        let other = procs.iter().find(|p| p.object != me).expect("coreaudiod has other clients");
        unsafe {
            let mut made = Vec::new();
            let mut tap_of = |object| {
                let desc = descriptions(&one(object)).unwrap().remove(0);
                create_tap(&desc, &mut made).unwrap()
            };
            let (uid_a, uid_b) = (tap_of(me), tap_of(other.object));
            let clock_uid = NSString::from_str(&clock.uid);
            let agg = create_aggregate(&clock_uid, &[&uid_a]).unwrap();
            let probe = Box::into_raw(Box::new(Probe::default()));
            let mut proc_id: AudioDeviceIOProcID = None;
            check(
                AudioDeviceCreateIOProcID(
                    agg,
                    Some(probe_proc),
                    probe.cast(),
                    NonNull::from(&mut proc_id),
                ),
                "probe IOProc",
            )
            .unwrap();
            check(AudioDeviceStart(agg, proc_id), "starting the probe").unwrap();
            let settle = || std::thread::sleep(Duration::from_millis(400));
            let report = |what: &str| {
                let p = &*probe;
                let gap = p.max_gap.swap(0, Ordering::Relaxed) as f64 / 1e6;
                let streams = get_vec::<AudioObjectID>(
                    agg,
                    kAudioDevicePropertyStreams,
                    kAudioObjectPropertyScopeInput,
                )
                .unwrap_or_default()
                .len();
                eprintln!(
                    "{what}: {} callbacks, {} buffers in the last, {streams} input streams, \
                     longest gap between callbacks {gap:.1} ms",
                    p.calls.load(Ordering::Relaxed),
                    p.buffers.load(Ordering::Relaxed),
                );
            };
            let set_taps = |uids: &[&NSString]| {
                let list = NSArray::from_slice(uids);
                let ptr = Retained::as_ptr(&list);
                let addr =
                    address(kAudioAggregateDevicePropertyTapList, kAudioObjectPropertyScopeGlobal);
                let t = Instant::now();
                let status = AudioObjectSetPropertyData(
                    agg,
                    NonNull::from(&addr),
                    0,
                    std::ptr::null(),
                    std::mem::size_of_val(&ptr) as u32,
                    NonNull::from(&ptr).cast(),
                );
                (status, t.elapsed())
            };
            settle();
            report("aggregate with tap A");
            let (status, took) = set_taps(&[&uid_a, &uid_b]);
            eprintln!(
                "adding tap B to the running aggregate: OSStatus {status}, {:.1} ms",
                ms(took)
            );
            settle();
            report("after adding B");
            let (status, took) = set_taps(&[&uid_a]);
            eprintln!("removing tap B: OSStatus {status}, {:.1} ms", ms(took));
            settle();
            report("after removing B");

            AudioDeviceStop(agg, proc_id);
            AudioDeviceDestroyIOProcID(agg, proc_id);
            drop(Box::from_raw(probe));
            AudioHardwareDestroyAggregateDevice(agg);
            for tap in made {
                AudioHardwareDestroyProcessTap(tap);
            }
        }
        drop(stream);
        assert_eq!(ours(), 0);
    }

    /// What the measuring IOProc counts.
    #[derive(Default)]
    struct Probe {
        calls: AtomicU64,
        buffers: AtomicU64,
        /// Host time of the last callback, and the longest gap seen (ns).
        last: AtomicU64,
        max_gap: AtomicU64,
    }

    unsafe extern "C-unwind" fn probe_proc(
        _device: AudioObjectID,
        now: NonNull<AudioTimeStamp>,
        input: NonNull<AudioBufferList>,
        _input_time: NonNull<AudioTimeStamp>,
        _output: NonNull<AudioBufferList>,
        _output_time: NonNull<AudioTimeStamp>,
        client: *mut c_void,
    ) -> OSStatus {
        let p = &*(client as *const Probe);
        p.calls.fetch_add(1, Ordering::Relaxed);
        p.buffers.store((*input.as_ptr()).mNumberBuffers as u64, Ordering::Relaxed);
        let mut tb = MachTimebaseInfo { numer: 1, denom: 1 };
        mach_timebase_info(&mut tb);
        let ns = now.as_ref().mHostTime as u128 * tb.numer as u128 / tb.denom.max(1) as u128;
        let last = p.last.swap(ns as u64, Ordering::Relaxed);
        if last != 0 {
            p.max_gap.fetch_max((ns as u64).saturating_sub(last), Ordering::Relaxed);
        }
        0
    }

    /// Creates a real tap on the default output and destroys it: the private
    /// aggregate must be gone afterwards. Ignored by default because it
    /// touches the machine's audio (and the very first time may raise the
    /// permission prompt): `cargo test -p simply-audio-capture -- --ignored`.
    #[test]
    #[ignore]
    fn a_tap_leaves_no_aggregate_device_behind() {
        let _serial = REAL_TAP.lock().unwrap_or_else(|e| e.into_inner());
        let ours = || list_devices().unwrap().iter().filter(|o| o.name == AGGREGATE_NAME).count();
        eprintln!("permission: {:?}", permission());
        let tap = OutputTap::start("", 48_000, 2, Box::new(|_: &[f32], _: u64| {})).unwrap();
        eprintln!("tapping {:?} at {} Hz", tap.device, tap.sample_rate);
        assert_eq!(ours(), 1, "the private aggregate is visible to its own process");
        drop(tap);
        assert_eq!(ours(), 0);
    }
}
