//! WasapiLoopback backend implementation.
//!
//! Captures system audio output (loopback) on Windows by re-exposing render endpoints
//! as input-only capture devices. This is the Windows equivalent of the macOS
//! ScreenCaptureKit host — it captures audio being played through speakers or headphones.
//!
//! Two capture modes are supported:
//!
//! 1. **Classic loopback** (default): Captures ALL audio going to a render endpoint
//!    using `AUDCLNT_STREAMFLAGS_LOOPBACK`.
//!
//! 2. **Per-process loopback** (when `StreamConfig.excluded_app_pids` or
//!    `excluded_app_names` is set): Uses `ActivateAudioInterfaceAsync` with
//!    `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` to exclude a specific process's
//!    audio. Requires Windows 10 version 2004 (build 19041) or later.

#[allow(unused_imports)]
pub use self::enumerate::{
    default_input_device, default_output_device, Devices, SupportedInputConfigs,
    SupportedOutputConfigs,
};
pub use self::stream::Stream;

use crate::traits::{DeviceTrait, HostTrait};
use crate::{
    BackendSpecificError, BuildStreamError, Data, DefaultStreamConfigError, DeviceDescription,
    DeviceDescriptionBuilder, DeviceDirection, DeviceId, DeviceIdError, DeviceNameError,
    DeviceType, DevicesError, InputCallbackInfo, InterfaceType, OutputCallbackInfo, SampleFormat,
    StreamConfig, StreamError, SupportedStreamConfig,
    SupportedStreamConfigRange, SupportedStreamConfigsError,
};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

// Reuse WASAPI infrastructure
use crate::host::wasapi::com;
use crate::host::wasapi::stream::{AudioClientFlow, StreamInner};

use windows::core::Interface;
use windows::core::GUID;
use windows::Win32::Devices::Properties;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::{Audio, KernelStreaming, Multimedia};
use windows::Win32::System::Com::{StructuredStorage, STGM_READ};
use windows::Win32::System::Variant::{VT_LPWSTR, VT_UI4};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

mod enumerate;
mod stream;

/// PKEY_AudioEndpoint_FormFactor (PID 0)
const PKEY_AUDIOENDPOINT_FORMFACTOR: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 0,
};

/// PKEY_AudioEndpoint_JackSubType (PID 8)
const PKEY_AUDIOENDPOINT_JACKSUBTYPE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 8,
};

// ============================================================================
// Host
// ============================================================================

/// The WasapiLoopback host captures system audio output on Windows.
///
/// Devices from this host represent render (output) endpoints re-exposed as
/// input-only loopback capture sources. This is analogous to ScreenCaptureKit
/// on macOS.
#[derive(Debug)]
pub struct Host;

impl Host {
    pub fn new() -> Result<Self, crate::HostUnavailable> {
        Ok(Host)
    }
}

impl HostTrait for Host {
    type Devices = Devices;
    type Device = Device;

    fn is_available() -> bool {
        true
    }

    fn devices(&self) -> Result<Self::Devices, DevicesError> {
        Devices::new()
    }

    fn default_input_device(&self) -> Option<Self::Device> {
        default_input_device()
    }

    fn default_output_device(&self) -> Option<Self::Device> {
        default_output_device()
    }
}

// ============================================================================
// Device
// ============================================================================

/// Wrapper because of that stupid decision to remove `Send` and `Sync` from raw pointers.
#[derive(Clone)]
struct IAudioClientWrapper(Audio::IAudioClient);
unsafe impl Send for IAudioClientWrapper {}
unsafe impl Sync for IAudioClientWrapper {}

/// A render endpoint re-exposed as a loopback capture device.
///
/// This wraps a WASAPI render (output) `IMMDevice` and presents it as an input-only
/// device that captures audio via WASAPI loopback mode.
#[derive(Clone)]
pub struct Device {
    device: Audio::IMMDevice,
    future_audio_client: Arc<Mutex<Option<IAudioClientWrapper>>>,
}

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    fn description(&self) -> Result<DeviceDescription, DeviceNameError> {
        Device::description(self)
    }

    fn id(&self) -> Result<DeviceId, DeviceIdError> {
        Device::id(self)
    }

    fn supports_input(&self) -> bool {
        true // Loopback devices are always input
    }

    fn supports_output(&self) -> bool {
        false // Loopback devices never do output
    }

    fn supported_input_configs(
        &self,
    ) -> Result<Self::SupportedInputConfigs, SupportedStreamConfigsError> {
        // Return the render endpoint's supported formats as input configs
        Device::supported_input_configs(self)
    }

    fn supported_output_configs(
        &self,
    ) -> Result<Self::SupportedOutputConfigs, SupportedStreamConfigsError> {
        Ok(vec![].into_iter())
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        Device::default_input_config(self)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        Err(DefaultStreamConfigError::StreamTypeNotSupported)
    }

    fn build_input_stream_raw<D, E>(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        let stream_inner = self.build_input_stream_raw_inner(config, sample_format)?;
        Ok(Stream::new_input(
            stream_inner,
            data_callback,
            error_callback,
        ))
    }

    fn build_output_stream_raw<D, E>(
        &self,
        _config: &StreamConfig,
        _sample_format: SampleFormat,
        _data_callback: D,
        _error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        Err(BuildStreamError::StreamConfigNotSupported)
    }
}

// ============================================================================
// Device implementation
// ============================================================================

/// Maps WASAPI FormFactor to a user-facing device type name for display.
/// Unlike the regular WASAPI backend, loopback devices always report as SystemAudioCapture.
fn form_factor_to_name(form_factor: u32) -> &'static str {
    match form_factor {
        0 => "Network Audio",   // RemoteNetworkDevice
        1 => "Speakers",        // Speakers
        2 => "Line Out",        // LineLevel
        3 => "Headphones",      // Headphones
        5 => "Headset",         // Headset
        8 => "S/PDIF",          // SPDIF
        9 => "HDMI Audio",      // DigitalAudioDisplayDevice
        _ => "Audio Output",
    }
}

/// Maps WASAPI EnumeratorName to InterfaceType.
fn enumerator_to_interface_type(enumerator: &str) -> Option<InterfaceType> {
    let typ = match enumerator.to_uppercase().as_str() {
        "HDAUDIO" => InterfaceType::BuiltIn,
        "USB" => InterfaceType::Usb,
        "BTHENUM" => InterfaceType::Bluetooth,
        "MMDEVAPI" | "SW" => InterfaceType::Virtual,
        _ => return None,
    };
    Some(typ)
}

/// Maps PKEY_AudioEndpoint_JackSubType GUID to InterfaceType.
fn jacksubtype_to_interface_type(guid_str: &str) -> Option<InterfaceType> {
    let guid_upper = guid_str.to_uppercase();
    let typ = match guid_upper.as_str() {
        "{D9E55EA0-0C89-4692-84FF-EB3C4B0D172F}" => InterfaceType::Hdmi,
        "{E47E4031-3EA6-418D-8F9B-B73843CCB2AD}" => InterfaceType::DisplayPort,
        "{DFF21CE1-F70F-11D0-B917-00A0C9223196}" => InterfaceType::Spdif,
        _ => return None,
    };
    Some(typ)
}

impl Device {
    pub(crate) fn from_immdevice(device: Audio::IMMDevice) -> Self {
        Device {
            device,
            future_audio_client: Arc::new(Mutex::new(None)),
        }
    }

    pub fn description(&self) -> Result<DeviceDescription, DeviceNameError> {
        unsafe {
            let property_store = self
                .device
                .OpenPropertyStore(STGM_READ)
                .expect("could not open property store");

            let friendly_name = get_property_string(
                &property_store,
                &Properties::DEVPKEY_Device_FriendlyName as *const _ as *const _,
            );

            let device_desc_str = get_property_string(
                &property_store,
                &Properties::DEVPKEY_Device_DeviceDesc as *const _ as *const _,
            );

            let interface_name = get_property_string(
                &property_store,
                &Properties::DEVPKEY_DeviceInterface_FriendlyName as *const _ as *const _,
            );

            let enumerator_name = get_property_string(
                &property_store,
                &Properties::DEVPKEY_Device_EnumeratorName as *const _ as *const _,
            );

            let form_factor = get_property_u32(
                &property_store,
                &PKEY_AUDIOENDPOINT_FORMFACTOR as *const _ as *const _,
            );

            let jack_subtype = get_property_string(
                &property_store,
                &PKEY_AUDIOENDPOINT_JACKSUBTYPE as *const _ as *const _,
            );

            // Build a descriptive name that includes the output device info
            let base_name = device_desc_str
                .or(friendly_name.clone())
                .ok_or_else(|| DeviceNameError::BackendSpecific {
                    err: BackendSpecificError {
                        description: "failed to retrieve device name".to_string(),
                    },
                })?;

            let form_factor_name = form_factor.map(form_factor_to_name).unwrap_or("Audio Output");
            let name = format!("{} ({})", base_name, form_factor_name);

            // Determine interface_type
            let mut interface_type = None;

            if let Some(ref enumerator) = enumerator_name {
                if let Some(itype) = enumerator_to_interface_type(enumerator) {
                    interface_type = Some(itype);
                }
            }

            if let Some(ref jack_guid) = jack_subtype {
                if let Some(itype) = jacksubtype_to_interface_type(jack_guid) {
                    interface_type = Some(itype);
                }
            }

            let mut builder = DeviceDescriptionBuilder::new(name)
                .direction(DeviceDirection::Input) // Loopback is always input
                .device_type(DeviceType::SystemAudioCapture);

            if let Some(itype) = interface_type {
                builder = builder.interface_type(itype);
            }

            if let Some(iface_name) = interface_name {
                builder = builder.driver(iface_name);
            }

            if let Some(fname) = friendly_name {
                builder = builder.add_extended_line(fname);
            }

            Ok(builder.build())
        }
    }

    fn id(&self) -> Result<DeviceId, DeviceIdError> {
        unsafe {
            match self.device.GetId() {
                Ok(pwstr) => match pwstr.to_string() {
                    Ok(id_str) => Ok(DeviceId(
                        crate::platform::HostId::WasapiLoopback,
                        id_str,
                    )),
                    Err(e) => Err(DeviceIdError::BackendSpecific {
                        err: BackendSpecificError {
                            description: format!(
                                "Failed to convert device ID to string: {}",
                                e
                            ),
                        },
                    }),
                },
                Err(e) => Err(DeviceIdError::BackendSpecific { err: e.into() }),
            }
        }
    }

    /// Ensures that `future_audio_client` contains a `Some` and returns a locked mutex to it.
    fn ensure_future_audio_client(
        &self,
    ) -> Result<MutexGuard<'_, Option<IAudioClientWrapper>>, windows::core::Error> {
        let mut lock = self.future_audio_client.lock().unwrap();
        if lock.is_some() {
            return Ok(lock);
        }

        let audio_client: Audio::IAudioClient = unsafe {
            self.device
                .Activate(windows::Win32::System::Com::CLSCTX_ALL, None)?
        };

        *lock = Some(IAudioClientWrapper(audio_client));
        Ok(lock)
    }

    /// Returns an uninitialized `IAudioClient`.
    fn build_audioclient(&self) -> Result<Audio::IAudioClient, windows::core::Error> {
        let mut lock = self.ensure_future_audio_client()?;
        Ok(lock.take().unwrap().0)
    }

    fn supported_input_configs(
        &self,
    ) -> Result<SupportedInputConfigs, SupportedStreamConfigsError> {
        // Return the render endpoint's formats as input configs for loopback
        self.supported_formats()
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        self.default_format()
    }

    fn default_format(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        com::com_initialized();

        let lock = match self.ensure_future_audio_client() {
            Ok(lock) => lock,
            Err(ref e) if e.code() == Audio::AUDCLNT_E_DEVICE_INVALIDATED => {
                return Err(DefaultStreamConfigError::DeviceNotAvailable)
            }
            Err(e) => {
                let description = format!("{}", e);
                let err = BackendSpecificError { description };
                return Err(err.into());
            }
        };
        let client = &lock.as_ref().unwrap().0;

        unsafe {
            let format_ptr = WaveFormatExPtr(
                client
                    .GetMixFormat()
                    .map_err(|e| {
                        let err: BackendSpecificError = e.into();
                        DefaultStreamConfigError::from(err)
                    })?,
            );

            format_from_waveformatex_ptr(format_ptr.0, client)
                .ok_or(DefaultStreamConfigError::StreamTypeNotSupported)
        }
    }

    fn supported_formats(&self) -> Result<SupportedInputConfigs, SupportedStreamConfigsError> {
        com::com_initialized();

        let lock = match self.ensure_future_audio_client() {
            Ok(lock) => lock,
            Err(ref e) if e.code() == Audio::AUDCLNT_E_DEVICE_INVALIDATED => {
                return Err(SupportedStreamConfigsError::DeviceNotAvailable)
            }
            Err(e) => {
                let description = format!("{}", e);
                let err = BackendSpecificError { description };
                return Err(err.into());
            }
        };
        let client = lock.as_ref().unwrap().0.clone();

        unsafe {
            let format_ptr = WaveFormatExPtr(client.GetMixFormat().map_err(|e| {
                let err: BackendSpecificError = e.into();
                SupportedStreamConfigsError::from(err)
            })?);

            match format_from_waveformatex_ptr(format_ptr.0, &client) {
                Some(config) => {
                    let config_range = SupportedStreamConfigRange {
                        channels: config.channels,
                        min_sample_rate: config.sample_rate,
                        max_sample_rate: config.sample_rate,
                        buffer_size: config.buffer_size,
                        sample_format: config.sample_format,
                    };
                    let mut supported_formats = vec![config_range];

                    // Also trial common sample rates
                    for &rate in crate::COMMON_SAMPLE_RATES {
                        if rate == config.sample_rate {
                            continue;
                        }
                        let test_format = config_to_waveformatextensible(
                            &StreamConfig {
                                channels: config.channels,
                                sample_rate: rate,
                                buffer_size: crate::BufferSize::Default,
                                excluded_app_pids: None,
                                excluded_app_names: None,
                                excluded_app_bundle_ids: None,
                            },
                            config.sample_format,
                        );
                        if let Some(ref test_fmt) = test_format {
                            if let Ok(true) =
                                crate::host::wasapi::device::is_format_supported(&client, &test_fmt.Format)
                            {
                                if let Some(tc) = format_from_waveformatex_ptr(&test_fmt.Format, &client) {
                                    supported_formats.push(SupportedStreamConfigRange {
                                        channels: tc.channels,
                                        min_sample_rate: tc.sample_rate,
                                        max_sample_rate: tc.sample_rate,
                                        buffer_size: tc.buffer_size,
                                        sample_format: tc.sample_format,
                                    });
                                }
                            }
                        }
                    }

                    Ok(supported_formats.into_iter())
                }
                None => Ok(vec![].into_iter()),
            }
        }
    }

    pub(crate) fn build_input_stream_raw_inner(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
    ) -> Result<StreamInner, BuildStreamError> {
        unsafe {
            com::com_initialized();

            let audio_client = match self.build_audioclient() {
                Ok(client) => client,
                Err(ref e) if e.code() == Audio::AUDCLNT_E_DEVICE_INVALIDATED => {
                    return Err(BuildStreamError::DeviceNotAvailable)
                }
                Err(e) => {
                    let description = format!("{}", e);
                    let err = BackendSpecificError { description };
                    return Err(err.into());
                }
            };

            let buffer_duration = buffer_size_to_duration(&config.buffer_size, config.sample_rate);

            // Always use loopback mode — this is the purpose of this host
            let stream_flags =
                Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK | Audio::AUDCLNT_STREAMFLAGS_LOOPBACK;

            let waveformatex = {
                let format_attempt = config_to_waveformatextensible(config, sample_format)
                    .ok_or(BuildStreamError::StreamConfigNotSupported)?;
                let share_mode = Audio::AUDCLNT_SHAREMODE_SHARED;

                match crate::host::wasapi::device::is_format_supported(
                    &audio_client,
                    &format_attempt.Format,
                ) {
                    Ok(false) => return Err(BuildStreamError::StreamConfigNotSupported),
                    Err(_) => return Err(BuildStreamError::DeviceNotAvailable),
                    _ => (),
                }

                let hresult = audio_client.Initialize(
                    share_mode,
                    stream_flags,
                    buffer_duration,
                    0,
                    &format_attempt.Format,
                    None,
                );
                match hresult {
                    Err(ref e) if e.code() == Audio::AUDCLNT_E_DEVICE_INVALIDATED => {
                        return Err(BuildStreamError::DeviceNotAvailable);
                    }
                    Err(e) => {
                        let description = format!("{}", e);
                        let err = BackendSpecificError { description };
                        return Err(err.into());
                    }
                    Ok(()) => (),
                };

                format_attempt.Format
            };

            let max_frames_in_buffer = audio_client
                .GetBufferSize()
                .map_err(|e| windows_err_to_cpal_err::<BuildStreamError>(e))?;

            let event = {
                use std::ptr;
                use windows::Win32::System::Threading;

                let event =
                    Threading::CreateEventA(None, false, false, windows::core::PCSTR(ptr::null()))
                        .map_err(|e| {
                            let description = format!("failed to create event: {}", e);
                            let err = BackendSpecificError { description };
                            BuildStreamError::from(err)
                        })?;

                if let Err(e) = audio_client.SetEventHandle(event) {
                    let description = format!("failed to call SetEventHandle: {}", e);
                    let err = BackendSpecificError { description };
                    return Err(err.into());
                }

                event
            };

            let capture_client = audio_client
                .GetService::<Audio::IAudioCaptureClient>()
                .map_err(|e| {
                    let description = format!("failed to build capture client: {}", e);
                    let err = BackendSpecificError { description };
                    BuildStreamError::from(err)
                })?;

            let client_flow = AudioClientFlow::Capture { capture_client };

            let audio_clock = audio_client
                .GetService::<Audio::IAudioClock>()
                .map_err(|e| {
                    let description = format!("failed to build audio clock: {}", e);
                    let err = BackendSpecificError { description };
                    BuildStreamError::from(err)
                })?;

            Ok(StreamInner {
                audio_client,
                audio_clock,
                client_flow,
                event,
                playing: false,
                max_frames_in_buffer,
                bytes_per_frame: waveformatex.nBlockAlign,
                config: config.clone(),
                sample_format,
            })
        }
    }
}

impl PartialEq for Device {
    fn eq(&self, other: &Device) -> bool {
        unsafe {
            struct IdRAII(windows::core::PWSTR);
            impl Drop for IdRAII {
                fn drop(&mut self) {
                    unsafe {
                        windows::Win32::System::Com::CoTaskMemFree(Some(self.0 .0 as *mut _))
                    }
                }
            }
            let id1 = self.device.GetId().expect("cpal: GetId failure");
            let id1 = IdRAII(id1);
            let id2 = other.device.GetId().expect("cpal: GetId failure");
            let id2 = IdRAII(id2);
            let mut offset = 0;
            loop {
                let w1: u16 = *(id1.0).0.offset(offset);
                let w2: u16 = *(id2.0).0.offset(offset);
                if w1 == 0 && w2 == 0 {
                    return true;
                }
                if w1 != w2 {
                    return false;
                }
                offset += 1;
            }
        }
    }
}

impl Eq for Device {}

impl std::hash::Hash for Device {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        unsafe {
            struct IdRAII(windows::core::PWSTR);
            impl Drop for IdRAII {
                fn drop(&mut self) {
                    unsafe {
                        windows::Win32::System::Com::CoTaskMemFree(Some(self.0 .0 as *mut _))
                    }
                }
            }
            let id = self.device.GetId().expect("cpal: GetId failure");
            let id = IdRAII(id);
            let mut offset = 0;
            loop {
                let w: u16 = *(id.0).0.offset(offset);
                if w == 0 {
                    break;
                }
                w.hash(state);
                offset += 1;
            }
        }
    }
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("WasapiLoopbackDevice")
            .field("device", &self.device)
            .field("description", &self.description())
            .finish()
    }
}

// ============================================================================
// Helper functions (same as wasapi/device.rs but scoped to this module)
// ============================================================================

// Use RAII to make sure CoTaskMemFree is called
struct WaveFormatExPtr(*mut Audio::WAVEFORMATEX);

impl Drop for WaveFormatExPtr {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::System::Com::CoTaskMemFree(Some(self.0 as *mut _));
        }
    }
}

unsafe fn format_from_waveformatex_ptr(
    waveformatex_ptr: *const Audio::WAVEFORMATEX,
    audio_client: &Audio::IAudioClient,
) -> Option<SupportedStreamConfig> {
    fn cmp_guid(a: &GUID, b: &GUID) -> bool {
        (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
    }
    let sample_format = match (
        (*waveformatex_ptr).wBitsPerSample,
        (*waveformatex_ptr).wFormatTag as u32,
    ) {
        (8, Audio::WAVE_FORMAT_PCM) => SampleFormat::U8,
        (16, Audio::WAVE_FORMAT_PCM) => SampleFormat::I16,
        (32, Multimedia::WAVE_FORMAT_IEEE_FLOAT) => SampleFormat::F32,
        (n_bits, KernelStreaming::WAVE_FORMAT_EXTENSIBLE) => {
            let waveformatextensible_ptr = waveformatex_ptr as *const Audio::WAVEFORMATEXTENSIBLE;
            let sub = (*waveformatextensible_ptr).SubFormat;

            if cmp_guid(&sub, &KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM) {
                match n_bits {
                    8 => SampleFormat::U8,
                    16 => SampleFormat::I16,
                    24 => SampleFormat::I24,
                    32 => SampleFormat::I32,
                    64 => SampleFormat::I64,
                    _ => return None,
                }
            } else if n_bits == 32
                && cmp_guid(&sub, &Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT)
            {
                SampleFormat::F32
            } else {
                return None;
            }
        }
        _ => return None,
    };

    let sample_rate = (*waveformatex_ptr).nSamplesPerSec;

    let (mut min_buffer_duration, mut max_buffer_duration) = (0, 0);
    let buffer_size_is_limited = audio_client
        .cast::<Audio::IAudioClient2>()
        .and_then(|ac| {
            ac.GetBufferSizeLimits(
                waveformatex_ptr,
                true,
                &mut min_buffer_duration,
                &mut max_buffer_duration,
            )
        })
        .is_ok();
    let buffer_size = if buffer_size_is_limited {
        crate::SupportedBufferSize::Range {
            min: buffer_duration_to_frames(min_buffer_duration, sample_rate),
            max: buffer_duration_to_frames(max_buffer_duration, sample_rate),
        }
    } else {
        crate::SupportedBufferSize::Range {
            min: 0,
            max: u32::MAX,
        }
    };

    Some(SupportedStreamConfig {
        channels: (*waveformatex_ptr).nChannels as _,
        sample_rate,
        buffer_size,
        sample_format,
        excluded_app_names: None,
        excluded_app_bundle_ids: None,
    })
}

fn config_to_waveformatextensible(
    config: &StreamConfig,
    sample_format: SampleFormat,
) -> Option<Audio::WAVEFORMATEXTENSIBLE> {
    let format_tag = match sample_format {
        SampleFormat::U8 | SampleFormat::I16 => Audio::WAVE_FORMAT_PCM,
        SampleFormat::I24 | SampleFormat::U24 | SampleFormat::I32 | SampleFormat::I64 | SampleFormat::F32 => {
            KernelStreaming::WAVE_FORMAT_EXTENSIBLE
        }
        _ => return None,
    };
    let channels = config.channels;
    let sample_rate = config.sample_rate;
    let sample_size = sample_format.sample_size() as u16;
    let block_align = channels * sample_size;
    let avg_bytes_per_sec = u32::from(block_align) * sample_rate;

    let sub_format = match sample_format {
        SampleFormat::U8 | SampleFormat::I16 | SampleFormat::I24 | SampleFormat::U24 | SampleFormat::I32 | SampleFormat::I64 => {
            KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM
        }
        SampleFormat::F32 => Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        _ => return None,
    };

    let waveformatex = Audio::WAVEFORMATEX {
        wFormatTag: format_tag as u16,
        nChannels: channels,
        nSamplesPerSec: sample_rate,
        nAvgBytesPerSec: avg_bytes_per_sec,
        nBlockAlign: block_align,
        wBitsPerSample: sample_size * 8,
        cbSize: if format_tag == Audio::WAVE_FORMAT_PCM {
            0
        } else {
            22
        },
    };

    let channel_mask = match channels {
        1 => KernelStreaming::SPEAKER_FRONT_CENTER,
        2 => KernelStreaming::SPEAKER_FRONT_LEFT | KernelStreaming::SPEAKER_FRONT_RIGHT,
        _ => 0,
    };

    Some(Audio::WAVEFORMATEXTENSIBLE {
        Format: waveformatex,
        Samples: Audio::WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: sample_size * 8,
        },
        dwChannelMask: channel_mask,
        SubFormat: sub_format,
    })
}

fn buffer_size_to_duration(buffer_size: &crate::BufferSize, sample_rate: u32) -> i64 {
    match buffer_size {
        crate::BufferSize::Fixed(frames) => {
            ((*frames as f64 / sample_rate as f64) * 10_000_000.0) as i64
        }
        crate::BufferSize::Default => 0,
    }
}

fn buffer_duration_to_frames(duration: i64, sample_rate: u32) -> crate::FrameCount {
    ((duration as f64 / 10_000_000.0) * sample_rate as f64) as crate::FrameCount
}

// Helper function to query a DWORD property from a WASAPI device property store
unsafe fn get_property_u32(
    property_store: &IPropertyStore,
    property_key: *const PROPERTYKEY,
) -> Option<u32> {
    let mut property_value = property_store.GetValue(property_key).ok()?;
    let prop_variant = &property_value.Anonymous.Anonymous;

    if prop_variant.vt != VT_UI4 {
        return None;
    }

    let value = *(&prop_variant.Anonymous as *const _ as *const u32);
    StructuredStorage::PropVariantClear(&mut property_value).ok();
    Some(value)
}

// Helper function to query a string property from a WASAPI device property store
unsafe fn get_property_string(
    property_store: &IPropertyStore,
    property_key: *const PROPERTYKEY,
) -> Option<String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::slice;

    let mut property_value = property_store.GetValue(property_key).ok()?;
    let prop_variant = &property_value.Anonymous.Anonymous;

    if prop_variant.vt != VT_LPWSTR {
        return None;
    }
    let ptr_utf16 = *(&prop_variant.Anonymous as *const _ as *const *const u16);

    const MAX_STRING_LEN: usize = 32768;
    let mut len = 0;
    while len < MAX_STRING_LEN && *ptr_utf16.add(len) != 0 {
        len += 1;
    }

    if len >= MAX_STRING_LEN {
        return None;
    }

    let string_slice = slice::from_raw_parts(ptr_utf16, len);
    let os_string: OsString = OsStringExt::from_wide(string_slice);
    let result = match os_string.into_string() {
        Ok(string) => Some(string),
        Err(os_string) => Some(os_string.to_string_lossy().into()),
    };

    StructuredStorage::PropVariantClear(&mut property_value).ok();
    result
}

// ============================================================================
// Error conversion helper
// ============================================================================

trait ErrDeviceNotAvailable: From<BackendSpecificError> {
    fn device_not_available() -> Self;
}

impl ErrDeviceNotAvailable for BuildStreamError {
    fn device_not_available() -> Self {
        Self::DeviceNotAvailable
    }
}

impl ErrDeviceNotAvailable for SupportedStreamConfigsError {
    fn device_not_available() -> Self {
        Self::DeviceNotAvailable
    }
}

fn windows_err_to_cpal_err<E: ErrDeviceNotAvailable>(e: windows::core::Error) -> E {
    match e.code() {
        Audio::AUDCLNT_E_DEVICE_INVALIDATED | Audio::AUDCLNT_E_DEVICE_IN_USE => {
            E::device_not_available()
        }
        _ => {
            let description = format!("{}", e);
            let err = BackendSpecificError { description };
            err.into()
        }
    }
}
