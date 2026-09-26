//! Higher-level reads over the CoreAudio HAL.
//!
//! Everything the state machine needs about the world arrives through
//! [`HalView`], so the machine itself can be tested against a fake.

use crate::sys::{self, AudioObjectId, PropertyAddress};
use std::collections::HashMap;

/// The two flags that are read on every single poll tick, and nothing else.
/// Kept separate from [`ProcessDetails`] because this is the hot path: it runs
/// for every audio process, several times a second, forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunningState {
    pub output: bool,
    pub input: bool,
}

/// Who a process is and where it is playing. Read only when a process is first
/// seen and when one of its flags changes -- never on an idle tick.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessDetails {
    pub pid: i32,
    pub bundle: Option<String>,
    /// The executable path, if it could be read. `None` once the process has
    /// exited -- which is why the state machine caches the first value it ever
    /// sees for a process object.
    pub exe: Option<String>,
    /// Names of the output devices the process is running on.
    pub devices: Vec<String>,
}

/// The source of truth about audio processes. Implemented for real by
/// [`CoreAudio`], and by a fake in the tests.
pub trait HalView {
    fn process_objects(&self) -> Vec<AudioObjectId>;
    /// The hot read: is this process running output or input right now?
    /// `None` when the object cannot be read at all.
    fn running(&self, object: AudioObjectId) -> Option<RunningState>;
    /// The cold read: identity and devices.
    fn details(&self, object: AudioObjectId) -> Option<ProcessDetails>;
}

/// One output device, for `audiowatch --devices`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub object: AudioObjectId,
    pub name: String,
    pub uid: Option<String>,
    pub output_channels: u32,
    pub input_channels: u32,
    pub running_somewhere: bool,
}

/// The live HAL, with a small cache of device names.
#[derive(Default)]
pub struct CoreAudio {
    device_names: std::cell::RefCell<HashMap<AudioObjectId, String>>,
}

impl CoreAudio {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn device_objects(&self) -> Vec<AudioObjectId> {
        sys::get_array::<AudioObjectId>(
            sys::SYSTEM_OBJECT,
            &PropertyAddress::global(sys::PROP_DEVICES),
        )
        .unwrap_or_default()
    }

    pub fn device_name(&self, device: AudioObjectId) -> String {
        if let Some(name) = self.device_names.borrow().get(&device) {
            return name.clone();
        }
        let name = sys::get_string(device, &PropertyAddress::global(sys::PROP_OBJECT_NAME))
            .unwrap_or_else(|| format!("device {device}"));
        self.device_names.borrow_mut().insert(device, name.clone());
        name
    }

    /// Total channels in a device's streams for one scope.
    pub fn device_channels(&self, device: AudioObjectId, scope: sys::Scope) -> u32 {
        let address = PropertyAddress::scoped(sys::PROP_STREAM_CONFIGURATION, scope);
        let bytes = match sys::get_array::<u8>(device, &address) {
            Ok(b) => b,
            Err(_) => return 0,
        };
        parse_buffer_list_channels(&bytes)
    }

    pub fn device_info(&self, device: AudioObjectId) -> DeviceInfo {
        DeviceInfo {
            object: device,
            name: self.device_name(device),
            uid: sys::get_string(device, &PropertyAddress::global(sys::PROP_DEVICE_UID)),
            output_channels: self.device_channels(device, sys::SCOPE_OUTPUT),
            input_channels: self.device_channels(device, sys::SCOPE_INPUT),
            running_somewhere: sys::get_u32_or(
                device,
                &PropertyAddress::global(sys::PROP_DEVICE_IS_RUNNING_SOMEWHERE),
                0,
            ) == 1,
        }
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.device_objects()
            .into_iter()
            .map(|d| self.device_info(d))
            .collect()
    }
}

impl HalView for CoreAudio {
    fn process_objects(&self) -> Vec<AudioObjectId> {
        sys::get_array::<AudioObjectId>(
            sys::SYSTEM_OBJECT,
            &PropertyAddress::global(sys::PROP_PROCESS_OBJECT_LIST),
        )
        .unwrap_or_default()
    }

    fn running(&self, object: AudioObjectId) -> Option<RunningState> {
        // Two property reads, and no `proc_pidpath` syscall. An unreadable
        // object answers `None` so the caller does not mistake it for idle.
        let output = sys::get::<u32>(
            object,
            &PropertyAddress::global(sys::PROP_PROCESS_IS_RUNNING_OUTPUT),
        )
        .ok()?;
        let input = sys::get_u32_or(
            object,
            &PropertyAddress::global(sys::PROP_PROCESS_IS_RUNNING_INPUT),
            0,
        );
        Some(RunningState {
            output: output == 1,
            input: input == 1,
        })
    }

    fn details(&self, object: AudioObjectId) -> Option<ProcessDetails> {
        let pid = sys::get::<i32>(object, &PropertyAddress::global(sys::PROP_PROCESS_PID)).ok()?;
        let devices = sys::get_array::<AudioObjectId>(
            object,
            &PropertyAddress::scoped(sys::PROP_PROCESS_DEVICES, sys::SCOPE_OUTPUT),
        )
        .unwrap_or_default()
        .into_iter()
        .filter(|d| *d != sys::OBJECT_UNKNOWN)
        .map(|d| self.device_name(d))
        .collect();
        Some(ProcessDetails {
            pid,
            bundle: sys::get_string(
                object,
                &PropertyAddress::global(sys::PROP_PROCESS_BUNDLE_ID),
            ),
            exe: sys::pid_path(pid),
            devices,
        })
    }
}

/// Sum `mNumberChannels` across an `AudioBufferList`'s buffers.
///
/// Laid out as `{ UInt32 mNumberBuffers; AudioBuffer mBuffers[1]; }` where
/// `AudioBuffer` is `{ UInt32 mNumberChannels; UInt32 mDataByteSize; void*
/// mData; }` — so the array starts at offset 8 on a 64-bit target, because the
/// pointer forces 8-byte alignment, and each entry is 16 bytes.
pub fn parse_buffer_list_channels(bytes: &[u8]) -> u32 {
    const LIST_HEADER: usize = 8;
    const BUFFER_SIZE: usize = 16;
    if bytes.len() < 4 {
        return 0;
    }
    let count = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let mut total = 0u32;
    for i in 0..count {
        let at = LIST_HEADER + i * BUFFER_SIZE;
        if at + 4 > bytes.len() {
            break;
        }
        let channels = u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        total = total.saturating_add(channels);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_list(channel_counts: &[u32]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(channel_counts.len() as u32).to_ne_bytes());
        v.extend_from_slice(&[0u8; 4]); // alignment padding
        for c in channel_counts {
            v.extend_from_slice(&c.to_ne_bytes());
            v.extend_from_slice(&0u32.to_ne_bytes()); // mDataByteSize
            v.extend_from_slice(&0u64.to_ne_bytes()); // mData
        }
        v
    }

    #[test]
    fn channels_sum_across_buffers() {
        assert_eq!(parse_buffer_list_channels(&buffer_list(&[2])), 2);
        assert_eq!(parse_buffer_list_channels(&buffer_list(&[1, 1, 1, 1])), 4);
        assert_eq!(parse_buffer_list_channels(&buffer_list(&[4, 2])), 6);
        assert_eq!(parse_buffer_list_channels(&buffer_list(&[])), 0);
    }

    #[test]
    fn a_truncated_buffer_list_does_not_panic() {
        let full = buffer_list(&[2, 2]);
        for cut in 0..full.len() {
            let _ = parse_buffer_list_channels(&full[..cut]);
        }
        // A count that lies about how many buffers follow is survivable.
        let mut lying = buffer_list(&[2]);
        lying[0] = 99;
        assert_eq!(parse_buffer_list_channels(&lying), 2);
    }

    #[test]
    fn the_real_hal_answers_with_a_plausible_process_list() {
        // An integration check against the live machine: coreaudiod always has
        // clients, and every one of them must report a pid.
        let hal = CoreAudio::new();
        let objects = hal.process_objects();
        assert!(
            !objects.is_empty(),
            "the HAL should always have client processes"
        );
        for o in &objects {
            // The hot read must work for every listed object.
            assert!(
                hal.running(*o).is_some(),
                "process object {o} would not report its flags"
            );
            if let Some(d) = hal.details(*o) {
                assert!(d.pid > 0, "process object {o} reported pid {}", d.pid);
            }
        }
    }

    #[test]
    fn the_real_hal_lists_output_devices_with_channel_counts() {
        let hal = CoreAudio::new();
        let devices = hal.devices();
        assert!(!devices.is_empty(), "this machine has audio devices");
        let with_output: Vec<_> = devices.iter().filter(|d| d.output_channels > 0).collect();
        assert!(
            !with_output.is_empty(),
            "at least one device should have output channels"
        );
        for d in &devices {
            assert!(!d.name.is_empty());
        }
    }
}
