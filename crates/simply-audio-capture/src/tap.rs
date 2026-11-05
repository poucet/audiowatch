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
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_core_audio::*;
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp, AudioTimeStampFlags};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};

use crate::apps::{
    self, AppInfo, AppStatus, AppTap, Backend, Follow, Mute, Proc, Stall, Target, STALL,
};
use crate::channels::{AppSinks, ChannelsStatus, Split};
use crate::default_out::{FollowDefault, Now, Outputs};
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
}

/// What a tap takes in.
pub(crate) enum Kind {
    /// Every process playing on this output device.
    Device(Device),
    /// The processes [`Target`] names, wherever they play, clocked by
    /// `clock` (the default output when the tap was made).
    Processes { target: Target, mute: Mute, clock: Device },
}

/// A running tap. Dropping it stops the IOProc and destroys the private
/// aggregate device and the tap, in that order.
pub(crate) struct OutputTap {
    tap: AudioObjectID,
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
            tap: 0,
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
        let tap_uid = create_tap(kind, &mut self.tap)?;
        self.aggregate = create_aggregate(&uid, &[&tap_uid])?;
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
            if self.tap != 0 {
                AudioHardwareDestroyProcessTap(self.tap);
            }
        }
    }
}

/// Make the process tap `kind` describes (stereo mixdown, private, with
/// its mute behaviour) into `*tap`, and return its UID. `*tap` is set
/// before anything can fail after it, so the caller destroys it either way.
unsafe fn create_tap(kind: &Kind, tap: &mut AudioObjectID) -> Result<Retained<NSString>, Error> {
    let desc = match kind {
        Kind::Device(d) => CATapDescription::initExcludingProcesses_andDeviceUID_withStream(
            CATapDescription::alloc(),
            &NSArray::<NSNumber>::new(),
            &NSString::from_str(&d.uid),
            0,
        ),
        Kind::Processes { target, .. } => {
            let ids = |objects: &[u32]| {
                let n: Vec<_> = objects.iter().map(|o| NSNumber::new_u32(*o)).collect();
                NSArray::from_retained_slice(&n)
            };
            match target {
                Target::Processes(o) => CATapDescription::initStereoMixdownOfProcesses(
                    CATapDescription::alloc(),
                    &ids(o),
                ),
                Target::AllExcept(o) => CATapDescription::initStereoGlobalTapButExcludeProcesses(
                    CATapDescription::alloc(),
                    &ids(o),
                ),
            }
        }
    };
    let mute = match kind {
        Kind::Device(_) | Kind::Processes { mute: Mute::Unmuted, .. } => CATapMuteBehavior::Unmuted,
        Kind::Processes { mute: Mute::Muted, .. } => CATapMuteBehavior::Muted,
        Kind::Processes { mute: Mute::MutedWhenTapped, .. } => CATapMuteBehavior::MutedWhenTapped,
    };
    desc.setName(&NSString::from_str(AGGREGATE_NAME));
    desc.setMixdown(true);
    desc.setMono(false);
    desc.setPrivate(true);
    desc.setMuteBehavior(mute);
    check(
        AudioHardwareCreateProcessTap(Some(&desc), tap),
        "creating the process tap (macOS 14.2+)",
    )?;
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
    ctx.calls.fetch_add(1, Ordering::Relaxed);
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
        target: &Target,
        mute: Mute,
        sink: Sink,
    ) -> Result<(OutputTap, u32), (Error, Sink)> {
        let clock = match find_output("") {
            Ok(d) => d,
            Err(e) => return Err((e, sink)),
        };
        let kind = Kind::Processes { target: target.clone(), mute, clock };
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

/// Wakes the follower: from HAL listeners (a process came or went, a device
/// list changed) or from its owner (stop).
#[derive(Default)]
struct Wake {
    flags: Mutex<(bool, bool)>, // (pending, stop)
    ready: Condvar,
}

impl Wake {
    fn poke(&self, stop: bool) {
        let mut f = self.flags.lock().unwrap_or_else(|e| e.into_inner());
        f.0 = true;
        f.1 |= stop;
        self.ready.notify_all();
    }

    /// Until poked or POLL passes; true = stop.
    fn wait(&self) -> bool {
        let mut f = self.flags.lock().unwrap_or_else(|e| e.into_inner());
        if !f.0 {
            f = self.ready.wait_timeout(f, POLL).unwrap_or_else(|e| e.into_inner()).0;
        }
        f.0 = false;
        f.1
    }
}

unsafe extern "C-unwind" fn on_change(
    _object: AudioObjectID,
    _count: u32,
    _addresses: NonNull<AudioObjectPropertyAddress>,
    client: *mut c_void,
) -> OSStatus {
    (*(client as *const Wake)).poke(false);
    0
}

/// A thread that ticks a follower: once at the start, then whenever one of
/// its HAL properties changes (or [`POLL`] passes), until it is dropped.
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
    /// Run `tick` on a thread called `name`. Returns after the first tick:
    /// its error is returned (and the thread ends); later ones are the
    /// follower's own to report.
    fn start<T>(
        name: &str,
        selectors: &'static [AudioObjectPropertySelector],
        mut tick: T,
    ) -> Result<Watcher, Error>
    where
        T: FnMut(Instant) -> Result<(), Error> + Send + 'static,
    {
        let wake = Arc::new(Wake::default());
        let (first_tx, first_rx) = mpsc::channel::<Result<(), Error>>();
        let thread = {
            let wake = Arc::clone(&wake);
            std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    let first = tick(Instant::now());
                    let failed = first.is_err();
                    let _ = first_tx.send(first);
                    if failed {
                        return;
                    }
                    while !wake.wait() {
                        let _ = tick(Instant::now());
                    }
                    // Dropping `tick` drops the follower, which destroys
                    // its tap.
                })
                .map_err(|e| {
                    Error::new(ErrorKind::Device, format!("cannot start the {name} thread: {e}"))
                })?
        };
        match first_rx.recv() {
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Ok(Ok(())) => {}
            Err(_) => {
                let _ = thread.join();
                return Err(Error::new(
                    ErrorKind::Device,
                    format!("the {name} thread died before reporting"),
                ));
            }
        }
        // A listener may still be running on a HAL thread as it is removed,
        // so the pointer it holds keeps its own reference, never released:
        // one small allocation per capture, never touched once freed.
        let client = Arc::into_raw(Arc::clone(&wake));
        for &selector in selectors {
            let addr = address(selector, kAudioObjectPropertyScopeGlobal);
            unsafe {
                AudioObjectAddPropertyListener(
                    kAudioObjectSystemObject as AudioObjectID,
                    NonNull::from(&addr),
                    Some(on_change),
                    client as *mut c_void,
                );
            }
        }
        Ok(Watcher { wake, client, selectors, thread: Some(thread) })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        for &selector in self.selectors {
            let addr = address(selector, kAudioObjectPropertyScopeGlobal);
            unsafe {
                AudioObjectRemovePropertyListener(
                    kAudioObjectSystemObject as AudioObjectID,
                    NonNull::from(&addr),
                    Some(on_change),
                    self.client as *mut c_void,
                );
            }
        }
        self.wake.poke(true);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What pokes an app follower: a process came or went, a device list
/// changed.
const APPS_WATCHED: [AudioObjectPropertySelector; 2] =
    [kAudioHardwarePropertyProcessObjectList, kAudioHardwarePropertyDevices];

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
        let status = Arc::new(Mutex::new(AppStatus::Failed { error: "starting".into() }));
        let hal = Hal { rate, channels, cache: HashMap::new(), devices: tap.needs_devices() };
        let mut follow = Follow::new(hal, tap, std::process::id() as i32, sink);
        let shared = Arc::clone(&status);
        let watcher = Watcher::start("audio app tap", &APPS_WATCHED, move |now| {
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
        let hal = Hal { rate, channels, cache: HashMap::new(), devices: tap.needs_devices() };
        let mut split = Split::new(hal, tap, std::process::id() as i32, sinks);
        let shared = Arc::clone(&status);
        let watcher = Watcher::start("audio app channels", &APPS_WATCHED, move |now| {
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
        let watcher = Watcher::start("audio default output tap", &DEFAULT_WATCHED, move |t| {
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
            target: Target::Processes(vec![object]),
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
            let (mut a, mut b) = (0, 0);
            let uid_a = create_tap(&one(me), &mut a).unwrap();
            let uid_b = create_tap(&one(other.object), &mut b).unwrap();
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
            AudioHardwareDestroyProcessTap(a);
            AudioHardwareDestroyProcessTap(b);
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
