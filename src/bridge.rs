//! **Unfinished.** The Objective-C bridge the process tap needs
//! (`CATapDescription` is an ObjC class with no C entry point). Merged onto
//! main at Chris's request while the tap itself is still being built, so
//! nothing here is wired up yet and all of it is dead — which is what the
//! module-level allow says, rather than silencing anything that is finished.
#![allow(dead_code)]

//! The minimum Objective-C and CoreFoundation needed to build a
//! `CATapDescription` and an aggregate-device description.
//!
//! `AudioHardwareCreateProcessTap` takes an Objective-C object and there is no C
//! equivalent, so this module reaches the runtime directly rather than pulling in
//! a bindings crate. Everything here is verified against the real classes at
//! startup by [`available`].

use std::ffi::{c_char, c_void, CString};

pub type Id = *mut c_void;
pub type Sel = *mut c_void;
pub type Class = *mut c_void;

#[link(name = "objc", kind = "dylib")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Class;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
}

/// `objc_msgSend` is declared with no signature on purpose: on arm64 it is not a
/// variadic call, so each use transmutes it to the exact signature of the method
/// being sent. Getting that signature wrong is undefined behaviour, so every
/// call site below is written out rather than generated.
fn msg_send_ptr() -> *const c_void {
    objc_msgSend as *const c_void
}

pub fn class(name: &str) -> Option<Class> {
    let c = CString::new(name).ok()?;
    // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
    let cls = unsafe { objc_getClass(c.as_ptr()) };
    if cls.is_null() {
        None
    } else {
        Some(cls)
    }
}

pub fn sel(name: &str) -> Sel {
    let c = CString::new(name).expect("selector name has no interior NUL");
    // SAFETY: `c` is a valid NUL-terminated string; the runtime copies it.
    unsafe { sel_registerName(c.as_ptr()) }
}

/// `[obj sel]` returning an object.
///
/// # Safety
/// `obj` must be a valid object (or Class) that responds to `s` with a method of
/// signature `-(id)sel`.
pub unsafe fn send0(obj: Id, s: Sel) -> Id {
    let f: extern "C" fn(Id, Sel) -> Id = std::mem::transmute(msg_send_ptr());
    f(obj, s)
}

/// `[obj sel:arg]` where `arg` is an object, returning an object.
///
/// # Safety
/// As [`send0`], with a method of signature `-(id)sel:(id)arg`.
pub unsafe fn send1_id(obj: Id, s: Sel, arg: Id) -> Id {
    let f: extern "C" fn(Id, Sel, Id) -> Id = std::mem::transmute(msg_send_ptr());
    f(obj, s, arg)
}

/// `[obj sel:arg]` where `arg` is a `BOOL`, returning nothing.
///
/// # Safety
/// As [`send0`], with a method of signature `-(void)sel:(BOOL)arg`.
pub unsafe fn send1_bool(obj: Id, s: Sel, arg: bool) {
    let f: extern "C" fn(Id, Sel, i8) = std::mem::transmute(msg_send_ptr());
    f(obj, s, i8::from(arg));
}

/// `[obj sel:arg]` where `arg` is an `NSInteger`, returning nothing.
///
/// # Safety
/// As [`send0`], with a method of signature `-(void)sel:(NSInteger)arg`.
pub unsafe fn send1_isize(obj: Id, s: Sel, arg: isize) {
    let f: extern "C" fn(Id, Sel, isize) = std::mem::transmute(msg_send_ptr());
    f(obj, s, arg);
}

/// `[NSNumber numberWithUnsignedInt:v]`, autoreleased.
pub fn ns_number_u32(v: u32) -> Option<Id> {
    let cls = class("NSNumber")?;
    // SAFETY: NSNumber responds to +numberWithUnsignedInt: with this signature.
    unsafe {
        let f: extern "C" fn(Id, Sel, u32) -> Id = std::mem::transmute(msg_send_ptr());
        let n = f(cls, sel("numberWithUnsignedInt:"), v);
        if n.is_null() {
            None
        } else {
            Some(n)
        }
    }
}

/// `[NSArray arrayWithObjects:count:]`, autoreleased.
pub fn ns_array(objects: &[Id]) -> Option<Id> {
    let cls = class("NSArray")?;
    // SAFETY: NSArray responds to +arrayWithObjects:count: with this signature,
    // and `objects` is valid for `objects.len()` elements.
    unsafe {
        let f: extern "C" fn(Id, Sel, *const Id, usize) -> Id = std::mem::transmute(msg_send_ptr());
        let a = f(
            cls,
            sel("arrayWithObjects:count:"),
            objects.as_ptr(),
            objects.len(),
        );
        if a.is_null() {
            None
        } else {
            Some(a)
        }
    }
}

/// Is the tap API actually present? Checked once at startup so a failure is
/// reported as "this macOS is too old" rather than as a crash.
pub fn available() -> bool {
    class("CATapDescription").is_some()
}

// ---------------------------------------------------------------------------
// CoreFoundation, for the aggregate-device description dictionary.
// ---------------------------------------------------------------------------

pub type CfTypeRef = *const c_void;
pub type CfStringRef = *const c_void;
pub type CfNumberRef = *const c_void;
pub type CfArrayRef = *const c_void;
pub type CfDictionaryRef = *const c_void;
pub type CfAllocatorRef = *const c_void;

const KCF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `kCFNumberSInt32Type`
const KCF_NUMBER_SINT32: isize = 3;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CfTypeRef);
    fn CFRetain(cf: CfTypeRef) -> CfTypeRef;
    fn CFStringCreateWithCString(
        alloc: CfAllocatorRef,
        cstr: *const c_char,
        encoding: u32,
    ) -> CfStringRef;
    fn CFStringGetCString(s: CfStringRef, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFNumberCreate(alloc: CfAllocatorRef, kind: isize, value: *const c_void) -> CfNumberRef;
    fn CFArrayCreate(
        alloc: CfAllocatorRef,
        values: *const CfTypeRef,
        count: isize,
        callbacks: *const c_void,
    ) -> CfArrayRef;
    fn CFDictionaryCreate(
        alloc: CfAllocatorRef,
        keys: *const CfTypeRef,
        values: *const CfTypeRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CfDictionaryRef;
    static kCFTypeArrayCallBacks: c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

/// An owned CoreFoundation object, released on drop.
pub struct CfOwned(CfTypeRef);

impl CfOwned {
    /// Take ownership of a +1 reference.
    pub fn from_create(r: CfTypeRef) -> Option<Self> {
        if r.is_null() {
            None
        } else {
            Some(Self(r))
        }
    }

    /// Take a +1 reference to an object we do not own.
    pub fn retain(r: CfTypeRef) -> Option<Self> {
        if r.is_null() {
            return None;
        }
        // SAFETY: `r` is a live CF object.
        Some(Self(unsafe { CFRetain(r) }))
    }

    pub fn get(&self) -> CfTypeRef {
        self.0
    }
}

impl Drop for CfOwned {
    fn drop(&mut self) {
        // SAFETY: we own exactly one reference, taken in the constructors.
        unsafe { CFRelease(self.0) };
    }
}

pub fn cf_string(s: &str) -> Option<CfOwned> {
    let c = CString::new(s).ok()?;
    // SAFETY: `c` is valid UTF-8 and NUL-terminated for the call's duration.
    CfOwned::from_create(unsafe {
        CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), KCF_STRING_ENCODING_UTF8)
    })
}

pub fn cf_string_to_rust(s: CfStringRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let mut buf = vec![0 as c_char; 1024];
    // SAFETY: `buf` is writable for its length; the function NUL-terminates.
    let ok = unsafe {
        CFStringGetCString(
            s,
            buf.as_mut_ptr(),
            buf.len() as isize,
            KCF_STRING_ENCODING_UTF8,
        )
    };
    if ok == 0 {
        return None;
    }
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8(bytes).ok()
}

pub fn cf_bool_as_number(v: bool) -> Option<CfOwned> {
    let n: i32 = i32::from(v);
    // SAFETY: `kind` matches the pointee type of `value`.
    CfOwned::from_create(unsafe {
        CFNumberCreate(
            std::ptr::null(),
            KCF_NUMBER_SINT32,
            (&n as *const i32).cast::<c_void>(),
        )
    })
}

pub fn cf_array(items: &[CfTypeRef]) -> Option<CfOwned> {
    // SAFETY: `items` is valid for its length; the type callbacks retain each entry.
    CfOwned::from_create(unsafe {
        CFArrayCreate(
            std::ptr::null(),
            items.as_ptr(),
            items.len() as isize,
            std::ptr::addr_of!(kCFTypeArrayCallBacks),
        )
    })
}

pub fn cf_dictionary(pairs: &[(CfTypeRef, CfTypeRef)]) -> Option<CfOwned> {
    let keys: Vec<CfTypeRef> = pairs.iter().map(|(k, _)| *k).collect();
    let values: Vec<CfTypeRef> = pairs.iter().map(|(_, v)| *v).collect();
    // SAFETY: both slices are valid for `pairs.len()`; the type callbacks retain.
    CfOwned::from_create(unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            pairs.len() as isize,
            std::ptr::addr_of!(kCFTypeDictionaryKeyCallBacks),
            std::ptr::addr_of!(kCFTypeDictionaryValueCallBacks),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tap_api_classes_exist_on_this_machine() {
        assert!(available(), "CATapDescription should exist on macOS 14.2+");
        assert!(class("NSNumber").is_some());
        assert!(class("NSArray").is_some());
        assert!(class("NoSuchClassAtAll").is_none());
    }

    #[test]
    fn selectors_resolve_and_are_interned() {
        let a = sel("initMonoMixdownOfProcesses:");
        let b = sel("initMonoMixdownOfProcesses:");
        assert!(!a.is_null());
        assert_eq!(a, b, "the runtime interns selectors");
    }

    #[test]
    fn ns_numbers_and_arrays_can_be_built() {
        let n = ns_number_u32(42).expect("NSNumber");
        let arr = ns_array(&[n]).expect("NSArray");
        assert!(!arr.is_null());
        assert!(ns_array(&[]).is_some(), "an empty array is legal");
    }

    #[test]
    fn cf_strings_round_trip() {
        let s = cf_string("BlackHole 2ch").expect("CFString");
        assert_eq!(cf_string_to_rust(s.get()).as_deref(), Some("BlackHole 2ch"));
        assert_eq!(cf_string_to_rust(std::ptr::null()), None);
        // A string with an interior NUL cannot be made, and says so rather than truncating.
        assert!(cf_string("bad\0string").is_none());
    }

    #[test]
    fn a_dictionary_of_the_shape_the_aggregate_needs_can_be_built() {
        let name = cf_string("audiowatch test").unwrap();
        let uid = cf_string("audiowatch-test-uid").unwrap();
        let private = cf_bool_as_number(true).unwrap();
        let sub_uid_key = cf_string("uid").unwrap();
        let tap_uid = cf_string("SOME-TAP-UUID").unwrap();
        let tap_entry = cf_dictionary(&[(sub_uid_key.get(), tap_uid.get())]).unwrap();
        let taps = cf_array(&[tap_entry.get()]).unwrap();
        let k_name = cf_string("name").unwrap();
        let k_uid = cf_string("uid").unwrap();
        let k_private = cf_string("private").unwrap();
        let k_taps = cf_string("taps").unwrap();
        let d = cf_dictionary(&[
            (k_name.get(), name.get()),
            (k_uid.get(), uid.get()),
            (k_private.get(), private.get()),
            (k_taps.get(), taps.get()),
        ]);
        assert!(
            d.is_some(),
            "the aggregate description dictionary should build"
        );
    }
}
