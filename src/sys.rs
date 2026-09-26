//! Hand-written declarations for the three system interfaces this tool needs:
//! CoreAudio's HAL, `libproc` for executable paths, and libc's local-time
//! conversion. Everything is verified against the macOS 26 SDK headers at
//! `CoreAudio.framework/Headers/AudioHardware.h`.

use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_long};

pub type OsStatus = i32;
pub type AudioObjectId = u32;
pub type Selector = u32;
pub type Scope = u32;
pub type Element = u32;

/// A four-character code, the way every CoreAudio selector is spelled.
pub const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

/// Render a selector back into its four characters, for error messages.
pub fn fourcc_str(v: u32) -> String {
    let b = [(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8];
    if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
        String::from_utf8_lossy(&b).into_owned()
    } else {
        format!("0x{v:08x}")
    }
}

pub const SYSTEM_OBJECT: AudioObjectId = 1;
pub const OBJECT_UNKNOWN: AudioObjectId = 0;

pub const SCOPE_GLOBAL: Scope = fourcc(b"glob");
pub const SCOPE_OUTPUT: Scope = fourcc(b"outp");
pub const SCOPE_INPUT: Scope = fourcc(b"inpt");
pub const ELEMENT_MAIN: Element = 0;

pub const PROP_OBJECT_NAME: Selector = fourcc(b"lnam");

// System object.
pub const PROP_PROCESS_OBJECT_LIST: Selector = fourcc(b"prs#");
pub const PROP_DEVICES: Selector = fourcc(b"dev#");

// Process objects (macOS 14.4+).
pub const PROP_PROCESS_PID: Selector = fourcc(b"ppid");
pub const PROP_PROCESS_BUNDLE_ID: Selector = fourcc(b"pbid");
pub const PROP_PROCESS_DEVICES: Selector = fourcc(b"pdv#");
pub const PROP_PROCESS_IS_RUNNING_INPUT: Selector = fourcc(b"piri");
pub const PROP_PROCESS_IS_RUNNING_OUTPUT: Selector = fourcc(b"piro");

// Devices.
pub const PROP_DEVICE_IS_RUNNING_SOMEWHERE: Selector = fourcc(b"gone");
pub const PROP_DEVICE_UID: Selector = fourcc(b"uid ");
pub const PROP_STREAM_CONFIGURATION: Selector = fourcc(b"slay");

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PropertyAddress {
    pub selector: Selector,
    pub scope: Scope,
    pub element: Element,
}

impl PropertyAddress {
    pub const fn global(selector: Selector) -> Self {
        Self {
            selector,
            scope: SCOPE_GLOBAL,
            element: ELEMENT_MAIN,
        }
    }
    pub const fn scoped(selector: Selector, scope: Scope) -> Self {
        Self {
            selector,
            scope,
            element: ELEMENT_MAIN,
        }
    }
}

pub type ListenerProc =
    extern "C" fn(AudioObjectId, u32, *const PropertyAddress, *mut c_void) -> OsStatus;

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: AudioObjectId,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
    ) -> OsStatus;

    fn AudioObjectGetPropertyData(
        object: AudioObjectId,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> OsStatus;

    fn AudioObjectAddPropertyListener(
        object: AudioObjectId,
        address: *const PropertyAddress,
        listener: ListenerProc,
        client_data: *mut c_void,
    ) -> OsStatus;
}

pub type CfTypeRef = *const c_void;
pub type CfStringRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CfTypeRef);
    fn CFStringGetCString(s: CfStringRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
}

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

extern "C" {
    fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
    fn mktime(tm: *mut Tm) -> i64;
}

/// `struct tm` as macOS lays it out, including the BSD `tm_gmtoff`/`tm_zone` tail.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct Tm {
    pub sec: c_int,
    pub min: c_int,
    pub hour: c_int,
    pub mday: c_int,
    pub mon: c_int,
    pub year: c_int,
    pub wday: c_int,
    pub yday: c_int,
    pub isdst: c_int,
    pub gmtoff: c_long,
    pub zone: *const c_char,
}

// `Tm::default()` needs a null `zone`; derive gives that for a raw pointer only
// via this manual impl, because *const c_char has no Default.
impl Tm {
    pub fn zeroed() -> Self {
        // SAFETY: `struct tm` is a plain-old-data struct; an all-zero value is
        // the documented starting point for filling one in before `mktime`.
        unsafe { std::mem::zeroed() }
    }
}

pub fn local_tm(epoch_secs: i64) -> Tm {
    let mut tm = Tm::zeroed();
    // SAFETY: both pointers are valid for the duration of the call.
    unsafe { localtime_r(&epoch_secs, &mut tm) };
    tm
}

/// Interpret a filled-in `Tm` as local wall-clock time and return epoch seconds.
pub fn local_epoch(mut tm: Tm) -> i64 {
    tm.isdst = -1;
    // SAFETY: `tm` is a valid, initialised `struct tm`.
    unsafe { mktime(&mut tm) }
}

/// The absolute path of a running process, or `None` if it has exited or is
/// not readable by this user.
pub fn pid_path(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    // PROC_PIDPATHINFO_MAXSIZE
    let mut buf = vec![0u8; 4096];
    // SAFETY: `buf` is writable for `buf.len()` bytes.
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok()
}

/// A CoreAudio call that failed, kept with enough context to be reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HalError {
    pub selector: Selector,
    pub status: OsStatus,
}

impl std::fmt::Display for HalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "CoreAudio property '{}' failed with status {} ('{}')",
            fourcc_str(self.selector),
            self.status,
            fourcc_str(self.status as u32)
        )
    }
}

impl std::error::Error for HalError {}

type HalResult<T> = Result<T, HalError>;

fn check(address: &PropertyAddress, status: OsStatus) -> HalResult<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(HalError {
            selector: address.selector,
            status,
        })
    }
}

/// Read a fixed-size property value.
pub fn get<T: Copy>(object: AudioObjectId, address: &PropertyAddress) -> HalResult<T> {
    let mut size = std::mem::size_of::<T>() as u32;
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    // SAFETY: `size` matches the buffer, which the HAL fills completely on success.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            address,
            0,
            std::ptr::null(),
            &mut size,
            value.as_mut_ptr().cast(),
        )
    };
    check(address, status)?;
    if size as usize != std::mem::size_of::<T>() {
        return Err(HalError {
            selector: address.selector,
            status: -1,
        });
    }
    // SAFETY: the HAL wrote a full `T`, as just verified by the size check.
    Ok(unsafe { value.assume_init() })
}

/// Read a `UInt32` property, treating any failure as "not reported".
pub fn get_u32_or(object: AudioObjectId, address: &PropertyAddress, fallback: u32) -> u32 {
    get::<u32>(object, address).unwrap_or(fallback)
}

/// Read a variable-length array property.
pub fn get_array<T: Copy + Default>(
    object: AudioObjectId,
    address: &PropertyAddress,
) -> HalResult<Vec<T>> {
    let mut size = 0u32;
    // SAFETY: `size` is a valid out-pointer.
    let status =
        unsafe { AudioObjectGetPropertyDataSize(object, address, 0, std::ptr::null(), &mut size) };
    check(address, status)?;
    let stride = std::mem::size_of::<T>();
    if (size as usize) < stride {
        return Ok(Vec::new());
    }
    let mut out = vec![T::default(); size as usize / stride];
    // SAFETY: `out` has exactly `size` bytes of capacity for the HAL to fill.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            address,
            0,
            std::ptr::null(),
            &mut size,
            out.as_mut_ptr().cast(),
        )
    };
    check(address, status)?;
    out.truncate(size as usize / stride);
    Ok(out)
}

/// Read a `CFStringRef` property and copy it into a Rust `String`.
pub fn get_string(object: AudioObjectId, address: &PropertyAddress) -> Option<String> {
    let cf: CfStringRef = get::<CfStringRef>(object, address).ok()?;
    if cf.is_null() {
        return None;
    }
    let mut buf = vec![0i8; 1024];
    // SAFETY: `cf` is a +1 CFString from the HAL; `buf` is writable for its length.
    let ok = unsafe {
        CFStringGetCString(
            cf,
            buf.as_mut_ptr(),
            buf.len() as isize,
            CF_STRING_ENCODING_UTF8,
        )
    };
    // SAFETY: the HAL documents this property as a +1 CFObject the caller releases.
    unsafe { CFRelease(cf) };
    if ok == 0 {
        return None;
    }
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    let s = String::from_utf8(bytes).ok()?;
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Register a listener. Returns the raw status so callers can decide whether a
/// failure is fatal; a duplicate registration answers `'nope'`.
pub fn add_listener(
    object: AudioObjectId,
    address: &PropertyAddress,
    listener: ListenerProc,
) -> OsStatus {
    // SAFETY: `listener` is a plain `extern "C" fn` with the documented
    // signature, and the HAL keeps no ownership of `address`.
    unsafe { AudioObjectAddPropertyListener(object, address, listener, std::ptr::null_mut()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_matches_the_header_spellings() {
        // Values lifted from AudioHardware.h so a typo in a selector is caught.
        assert_eq!(PROP_PROCESS_OBJECT_LIST, 0x7072_7323);
        assert_eq!(PROP_PROCESS_PID, 0x7070_6964);
        assert_eq!(PROP_PROCESS_IS_RUNNING_OUTPUT, 0x7069_726f);
        assert_eq!(PROP_DEVICE_IS_RUNNING_SOMEWHERE, 0x676f_6e65);
        assert_eq!(SCOPE_GLOBAL, 0x676c_6f62);
    }

    #[test]
    fn fourcc_str_round_trips_printable_codes() {
        assert_eq!(fourcc_str(PROP_PROCESS_IS_RUNNING_OUTPUT), "piro");
        assert_eq!(fourcc_str(PROP_DEVICE_IS_RUNNING_SOMEWHERE), "gone");
        assert_eq!(fourcc_str(0x0000_0001), "0x00000001");
    }

    #[test]
    fn local_time_round_trips_through_mktime() {
        let now = 1_790_000_000i64;
        let tm = local_tm(now);
        assert_eq!(local_epoch(tm), now);
    }

    #[test]
    fn pid_path_finds_this_process_and_rejects_nonsense() {
        let me = pid_path(std::process::id() as i32).expect("own path");
        assert!(
            me.contains("audiowatch") || me.contains("sys-"),
            "unexpected path {me}"
        );
        assert_eq!(pid_path(0), None);
        assert_eq!(pid_path(-1), None);
    }
}
