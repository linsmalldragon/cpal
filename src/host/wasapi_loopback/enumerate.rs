use std::vec::IntoIter as VecIntoIter;

use crate::{BackendSpecificError, DevicesError, SupportedStreamConfigRange};

use super::Device;

// Reuse the WASAPI COM initialization and enumerator infrastructure
use crate::host::wasapi::com;
use windows::Win32::Media::Audio;
use windows::Win32::System::Com;

use std::sync::OnceLock;

/// Send/Sync wrapper around `IMMDeviceEnumerator`.
struct Enumerator(Audio::IMMDeviceEnumerator);

unsafe impl Send for Enumerator {}
unsafe impl Sync for Enumerator {}

static ENUMERATOR: OnceLock<Enumerator> = OnceLock::new();

fn get_enumerator() -> &'static Enumerator {
    ENUMERATOR.get_or_init(|| {
        com::com_initialized();
        unsafe {
            let enumerator = Com::CoCreateInstance::<_, Audio::IMMDeviceEnumerator>(
                &Audio::MMDeviceEnumerator,
                None,
                Com::CLSCTX_ALL,
            )
            .unwrap();
            Enumerator(enumerator)
        }
    })
}

/// WASAPI Loopback device iterator.
///
/// Enumerates render (output) endpoints and wraps them as loopback capture devices.
pub struct Devices {
    collection: Audio::IMMDeviceCollection,
    total_count: u32,
    next_item: u32,
}

impl Devices {
    pub fn new() -> Result<Self, DevicesError> {
        unsafe {
            // Enumerate only render (output) endpoints — these become loopback capture sources
            let collection = get_enumerator()
                .0
                .EnumAudioEndpoints(Audio::eRender, Audio::DEVICE_STATE_ACTIVE)
                .map_err(BackendSpecificError::from)?;

            let count = collection.GetCount().map_err(BackendSpecificError::from)?;

            Ok(Devices {
                collection,
                total_count: count,
                next_item: 0,
            })
        }
    }
}

unsafe impl Send for Devices {}
unsafe impl Sync for Devices {}

impl Iterator for Devices {
    type Item = Device;

    fn next(&mut self) -> Option<Device> {
        if self.next_item >= self.total_count {
            return None;
        }

        unsafe {
            let device = self.collection.Item(self.next_item).unwrap();
            self.next_item += 1;
            Some(Device::from_immdevice(device))
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let num = (self.total_count - self.next_item) as usize;
        (num, Some(num))
    }
}

pub fn default_input_device() -> Option<Device> {
    unsafe {
        let device = get_enumerator()
            .0
            .GetDefaultAudioEndpoint(Audio::eRender, Audio::eConsole)
            .ok()?;
        Some(Device::from_immdevice(device))
    }
}

pub fn default_output_device() -> Option<Device> {
    // Loopback host has no output devices — it only provides capture
    None
}

pub type SupportedInputConfigs = VecIntoIter<SupportedStreamConfigRange>;
pub type SupportedOutputConfigs = VecIntoIter<SupportedStreamConfigRange>;
