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
use windows::Win32::Foundation;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::{Audio, KernelStreaming, Multimedia};
use windows::Win32::System::Com::{StructuredStorage, STGM_READ};
use windows::Win32::System::Variant::{VT_BLOB, VT_LPWSTR};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

mod enumerate;
mod process;
mod stream;

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
        let excluded_pids = resolve_excluded_pids(config);

        // Create a silence render stream to keep the WASAPI audio engine active.
        // Without this, loopback capture only produces samples when real audio is
        // playing, causing long delays in the audio pipeline. The silence stream
        // ensures continuous sample delivery (zeros when silent), matching macOS
        // ScreenCaptureKit behavior.
        //
        // EXCEPTION: Skip for Bluetooth endpoints. The additional render client
        // (IAudioClient in shared render mode) on a BT A2DP endpoint can trigger
        // codec renegotiation or quality degradation. When using BT headphones,
        // the user is actively playing audio, so the audio engine is already active.
        let silence = if self.is_bluetooth() {
            eprintln!(
                "cpal: skipping silence render stream for Bluetooth endpoint \
                 (loopback will only produce samples when audio is playing)"
            );
            None
        } else {
            match create_silence_render_components(&self.device) {
                Ok(components) => Some(stream::SilenceStream::new(components)),
                Err(e) => {
                    eprintln!(
                        "cpal: failed to create silence render stream \
                         (loopback will only produce samples when audio is playing): {:?}",
                        e
                    );
                    None
                }
            }
        };

        if excluded_pids.len() >= 2 {
            // Multi-PID audio subtraction path:
            // Total (classic loopback) - Include_0 - Include_1 - ... = output
            com::com_initialized();

            let stream_flags_loopback = Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK | Audio::AUDCLNT_STREAMFLAGS_LOOPBACK;
            let stream_flags_process = Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK;

            // Create Total capture (classic loopback on this render endpoint)
            let total_audio_client = match self.build_audioclient() {
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
            let total = initialize_capture_client(total_audio_client, config, sample_format, stream_flags_loopback)?;

            // Create Include captures for each excluded PID
            let mut includes = Vec::new();
            for &pid in &excluded_pids {
                match activate_process_loopback_audio_client(pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE) {
                    Ok(include_client) => {
                        match initialize_capture_client(include_client, config, sample_format, stream_flags_process) {
                            Ok(components) => includes.push(components),
                            Err(e) => {
                                // Log but continue - some processes may not be capturable
                                eprintln!("Warning: failed to initialize include capture for PID {}: {:?}", pid, e);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Warning: failed to activate include capture for PID {}: {:?}", pid, e);
                    }
                }
            }

            if includes.is_empty() {
                // All include captures failed, fall back to classic loopback (pass through Total)
                // We need to rebuild as a StreamInner for the Single path
                // since the total's audio_client is already initialized
                let client_flow = AudioClientFlow::Capture { capture_client: total.capture_client };
                let audio_clock = unsafe {
                    total.audio_client
                        .GetService::<Audio::IAudioClock>()
                        .map_err(|e| {
                            let description = format!("failed to build audio clock: {}", e);
                            let err = BackendSpecificError { description };
                            BuildStreamError::from(err)
                        })?
                };
                let stream_inner = StreamInner {
                    audio_client: total.audio_client,
                    audio_clock,
                    client_flow,
                    event: total.event,
                    playing: false,
                    max_frames_in_buffer: total.max_frames_in_buffer,
                    bytes_per_frame: total.bytes_per_frame,
                    config: config.clone(),
                    sample_format,
                };
                return Ok(Stream::new_input(stream_inner, data_callback, error_callback, silence));
            }

            let bytes_per_frame = total.bytes_per_frame;
            Ok(Stream::new_multi_exclude(
                total,
                includes,
                sample_format,
                bytes_per_frame,
                data_callback,
                error_callback,
                silence,
            ))
        } else {
            // 0 or 1 PID: classic loopback or single-PID EXCLUDE
            let stream_inner = self.build_input_stream_raw_inner(config, sample_format, &excluded_pids)?;
            Ok(Stream::new_input(
                stream_inner,
                data_callback,
                error_callback,
                silence,
            ))
        }
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

/// Get the computer's hostname for use in device display names.
/// Returns something like "DESKTOP-ABC123" or a custom computer name.
/// Falls back to "PC" if the API call fails.
///
/// This mirrors the macOS ScreenCaptureKit approach where loopback devices
/// are named "{hostname} Audio" instead of using driver/hardware names.
fn get_computer_name() -> String {
    use windows::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
    unsafe {
        let mut size: u32 = 0;
        // First call to get required buffer size (expected to fail with ERROR_MORE_DATA)
        let _ = GetComputerNameExW(ComputerNameDnsHostname, None, &mut size);
        if size == 0 {
            return "PC".to_string();
        }
        let mut buffer = vec![0u16; size as usize];
        let result = GetComputerNameExW(
            ComputerNameDnsHostname,
            Some(windows::core::PWSTR(buffer.as_mut_ptr())),
            &mut size,
        );
        if result.is_ok() {
            String::from_utf16_lossy(&buffer[..size as usize])
        } else {
            "PC".to_string()
        }
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

            let interface_name = get_property_string(
                &property_store,
                &Properties::DEVPKEY_DeviceInterface_FriendlyName as *const _ as *const _,
            );

            let enumerator_name = get_property_string(
                &property_store,
                &Properties::DEVPKEY_Device_EnumeratorName as *const _ as *const _,
            );

            let jack_subtype = get_property_string(
                &property_store,
                &PKEY_AUDIOENDPOINT_JACKSUBTYPE as *const _ as *const _,
            );

            // Build a descriptive name using computer hostname (matching macOS ScreenCaptureKit style)
            // macOS: "{hostname} Audio"  (e.g., "xiaodragonmacbook Audio")
            // Windows: "{hostname} Audio" (e.g., "DESKTOP-ABC123 Audio")
            let computer_name = get_computer_name();
            let name = format!("{} Audio", computer_name);

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

    /// Check if this device is connected via Bluetooth.
    /// Used to skip the silence render stream for BT endpoints,
    /// as the additional render client can degrade BT audio quality
    /// (e.g., triggering A2DP → HFP codec switch or parameter renegotiation).
    fn is_bluetooth(&self) -> bool {
        unsafe {
            let property_store = match self.device.OpenPropertyStore(STGM_READ) {
                Ok(store) => store,
                Err(_) => return false,
            };
            let enumerator_name = get_property_string(
                &property_store,
                &Properties::DEVPKEY_Device_EnumeratorName as *const _ as *const _,
            );
            matches!(enumerator_name.as_deref(), Some(name) if name.eq_ignore_ascii_case("BTHENUM"))
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
        excluded_pids: &[u32],
    ) -> Result<StreamInner, BuildStreamError> {
        unsafe {
            com::com_initialized();

            let audio_client = if excluded_pids.len() == 1 {
                // Single PID: per-process loopback EXCLUDE path
                let pid = excluded_pids[0];
                activate_process_loopback_audio_client(pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE)?
            } else {
                // Classic loopback path (also used as Total for multi-PID subtraction)
                match self.build_audioclient() {
                    Ok(client) => client,
                    Err(ref e) if e.code() == Audio::AUDCLNT_E_DEVICE_INVALIDATED => {
                        return Err(BuildStreamError::DeviceNotAvailable)
                    }
                    Err(e) => {
                        let description = format!("{}", e);
                        let err = BackendSpecificError { description };
                        return Err(err.into());
                    }
                }
            };

            // Per-process loopback (single PID) does NOT use AUDCLNT_STREAMFLAGS_LOOPBACK
            // (the loopback is set up at activation level).
            // Classic loopback and multi-PID subtraction require the LOOPBACK flag on the Total capture.
            let stream_flags = if excluded_pids.len() == 1 {
                Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            } else {
                Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK | Audio::AUDCLNT_STREAMFLAGS_LOOPBACK
            };

            // Use the shared helper to initialize the capture client
            let components = initialize_capture_client(audio_client, config, sample_format, stream_flags)?;

            let client_flow = AudioClientFlow::Capture { capture_client: components.capture_client };

            let audio_clock = components.audio_client
                .GetService::<Audio::IAudioClock>()
                .map_err(|e| {
                    let description = format!("failed to build audio clock: {}", e);
                    let err = BackendSpecificError { description };
                    BuildStreamError::from(err)
                })?;

            Ok(StreamInner {
                audio_client: components.audio_client,
                audio_clock,
                client_flow,
                event: components.event,
                playing: false,
                max_frames_in_buffer: components.max_frames_in_buffer,
                bytes_per_frame: components.bytes_per_frame,
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
// Capture client initialization helpers
// ============================================================================

/// Components of an initialized capture client, ready for audio capture.
///
/// Used by both the classic loopback path and the multi-PID subtraction path
/// to share the IAudioClient → capture client initialization logic.
pub(crate) struct CaptureComponents {
    pub audio_client: Audio::IAudioClient,
    pub capture_client: Audio::IAudioCaptureClient,
    pub event: Foundation::HANDLE,
    pub max_frames_in_buffer: u32,
    pub bytes_per_frame: u16,
}

// SAFETY: CaptureComponents contains Windows COM objects and HANDLE, which are safe to send
// between threads. COM objects are reference-counted and HANDLE is a synchronization primitive.
// This is the same reasoning used for wasapi::stream::Stream and wasapi_loopback::Stream.
unsafe impl Send for CaptureComponents {}

/// Initialize an `IAudioClient` for capture, setting up the event, format, and capture client.
///
/// The caller provides a pre-obtained `IAudioClient` (from either `IMMDevice::Activate` or
/// `ActivateAudioInterfaceAsync`). This function handles:
/// 1. Format negotiation and `Initialize()`
/// 2. Event creation and `SetEventHandle()`
/// 3. `GetService::<IAudioCaptureClient>()`
///
/// # Arguments
/// * `audio_client` - A pre-obtained `IAudioClient`
/// * `config` - Stream configuration (sample rate, channels, buffer size)
/// * `sample_format` - The desired sample format
/// * `stream_flags` - WASAPI stream flags (e.g., with/without LOOPBACK)
pub(crate) fn initialize_capture_client(
    audio_client: Audio::IAudioClient,
    config: &StreamConfig,
    sample_format: SampleFormat,
    stream_flags: u32,
) -> Result<CaptureComponents, BuildStreamError> {
    unsafe {
        let buffer_duration = buffer_size_to_duration(&config.buffer_size, config.sample_rate);

        let waveformatex = {
            let format_attempt = config_to_waveformatextensible(config, sample_format)
                .ok_or(BuildStreamError::StreamConfigNotSupported)?;

            match crate::host::wasapi::device::is_format_supported(
                &audio_client,
                &format_attempt.Format,
            ) {
                Ok(false) => return Err(BuildStreamError::StreamConfigNotSupported),
                Err(_) => return Err(BuildStreamError::DeviceNotAvailable),
                _ => (),
            }

            let hresult = audio_client.Initialize(
                Audio::AUDCLNT_SHAREMODE_SHARED,
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
                let _ = Foundation::CloseHandle(event);
                let description = format!("failed to call SetEventHandle: {}", e);
                let err = BackendSpecificError { description };
                return Err(err.into());
            }

            event
        };

        let capture_client = audio_client
            .GetService::<Audio::IAudioCaptureClient>()
            .map_err(|e| {
                let _ = Foundation::CloseHandle(event);
                let description = format!("failed to build capture client: {}", e);
                let err = BackendSpecificError { description };
                BuildStreamError::from(err)
            })?;

        Ok(CaptureComponents {
            audio_client,
            capture_client,
            event,
            max_frames_in_buffer,
            bytes_per_frame: waveformatex.nBlockAlign,
        })
    }
}

// ============================================================================
// Silence render stream (keeps WASAPI audio engine active for loopback)
// ============================================================================

/// Create render components for a silence stream on the given endpoint.
///
/// WASAPI loopback capture only produces sample callbacks when the audio engine
/// is active (i.e., at least one application is rendering audio to the endpoint).
/// By creating a render client that continuously outputs silence, we keep the
/// engine active and ensure the loopback capture delivers zero-valued samples
/// even when no real audio is playing — matching macOS ScreenCaptureKit behavior.
///
/// The silence is rendered using `AUDCLNT_BUFFERFLAGS_SILENT`, which tells the
/// engine to treat the buffer as silence without writing actual data. This has
/// near-zero CPU overhead.
fn create_silence_render_components(
    device: &Audio::IMMDevice,
) -> Result<stream::SilenceRenderComponents, BuildStreamError> {
    unsafe {
        com::com_initialized();

        // Create a separate IAudioClient for rendering (independent from the capture client)
        let audio_client: Audio::IAudioClient = device
            .Activate(windows::Win32::System::Com::CLSCTX_ALL, None)
            .map_err(|e| {
                let description = format!("failed to activate render client for silence stream: {}", e);
                BuildStreamError::from(BackendSpecificError { description })
            })?;

        // Get the device's mix format
        let format_ptr = audio_client.GetMixFormat().map_err(|e| {
            let description = format!("failed to get mix format for silence stream: {}", e);
            BuildStreamError::from(BackendSpecificError { description })
        })?;
        // Wrap in RAII so CoTaskMemFree is called on all exit paths
        let _format_guard = WaveFormatExPtr(format_ptr);

        // Initialize as shared-mode render stream with event-driven buffer notifications
        let hresult = audio_client.Initialize(
            Audio::AUDCLNT_SHAREMODE_SHARED,
            Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0, // default buffer duration
            0,
            format_ptr,
            None,
        );
        if let Err(e) = hresult {
            let description = format!("failed to initialize silence render client: {}", e);
            return Err(BuildStreamError::from(BackendSpecificError { description }));
        }

        let buffer_frames = audio_client.GetBufferSize().map_err(|e| {
            let description = format!("failed to get buffer size for silence stream: {}", e);
            BuildStreamError::from(BackendSpecificError { description })
        })?;

        // Create event for buffer-ready notifications
        use windows::Win32::System::Threading;
        let event = Threading::CreateEventA(None, false, false, windows::core::PCSTR(std::ptr::null()))
            .map_err(|e| {
                let description = format!("failed to create event for silence stream: {}", e);
                BuildStreamError::from(BackendSpecificError { description })
            })?;

        if let Err(e) = audio_client.SetEventHandle(event) {
            let _ = Foundation::CloseHandle(event);
            let description = format!("failed to set event handle for silence stream: {}", e);
            return Err(BuildStreamError::from(BackendSpecificError { description }));
        }

        // Get the render client (for writing silence buffers)
        let render_client = audio_client
            .GetService::<Audio::IAudioRenderClient>()
            .map_err(|e| {
                let _ = Foundation::CloseHandle(event);
                let description = format!("failed to get render client for silence stream: {}", e);
                BuildStreamError::from(BackendSpecificError { description })
            })?;

        Ok(stream::SilenceRenderComponents {
            audio_client,
            render_client,
            event,
            buffer_frames,
        })
    }
}

// ============================================================================
// Per-process loopback activation (Windows 10 2004+)
// ============================================================================

// Manual definitions for process loopback types not yet in the `windows` crate.

/// The virtual audio device path for process loopback activation.
const VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK: &str = "VAD\\Process_Loopback";

/// Activation type for process loopback.
const AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK: u32 = 1;

/// Include only the target process's audio.
const PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE: u32 = 0;

/// Exclude the target process's audio (capture everything else).
const PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE: u32 = 1;

#[repr(C)]
struct AudioClientProcessLoopbackParams {
    target_process_id: u32,
    process_loopback_mode: u32,
}

#[repr(C)]
struct AudioClientActivationParams {
    activation_type: u32,
    process_loopback_params: AudioClientProcessLoopbackParams,
}

/// COM completion handler for `ActivateAudioInterfaceAsync`.
///
/// Implements `IActivateAudioInterfaceCompletionHandler` and signals completion
/// via a oneshot channel.
#[windows::core::implement(Audio::IActivateAudioInterfaceCompletionHandler)]
struct ActivationCompletionHandler {
    tx: std::sync::Mutex<Option<std::sync::mpsc::Sender<Result<Audio::IAudioClient, windows::core::Error>>>>,
}

impl Audio::IActivateAudioInterfaceCompletionHandler_Impl for ActivationCompletionHandler_Impl {
    fn ActivateCompleted(
        &self,
        activate_operation: windows::core::Ref<'_, Audio::IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let result = unsafe {
            let mut hr = windows::Win32::Foundation::E_FAIL;
            let mut activated_interface: Option<windows::core::IUnknown> = None;
            let op: Audio::IActivateAudioInterfaceAsyncOperation = activate_operation.clone().unwrap();
            op.GetActivateResult(&mut hr, &mut activated_interface)?;
            hr.ok()?;
            match activated_interface {
                Some(intf) => intf.cast::<Audio::IAudioClient>(),
                None => Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_POINTER,
                    "ActivateAudioInterfaceAsync returned null interface",
                )),
            }
        };

        if let Ok(mut tx_guard) = self.tx.lock() {
            if let Some(tx) = tx_guard.take() {
                let _ = tx.send(result);
            }
        }
        Ok(())
    }
}

/// Activate an `IAudioClient` for per-process loopback capture.
///
/// Uses `ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`
/// to get an audio client that captures audio relative to the target process.
///
/// # Arguments
/// * `target_pid` - The process ID to target
/// * `process_loopback_mode` - Use `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE` to capture
///   everything except the target process, or `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`
///   to capture only the target process's audio.
///
/// Requires Windows 10 version 2004 (build 19041) or later.
fn activate_process_loopback_audio_client(
    target_pid: u32,
    process_loopback_mode: u32,
) -> Result<Audio::IAudioClient, BuildStreamError> {
    use std::sync::mpsc;

    com::com_initialized();

    // Set up activation params
    let mut activation_params = AudioClientActivationParams {
        activation_type: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        process_loopback_params: AudioClientProcessLoopbackParams {
            target_process_id: target_pid,
            process_loopback_mode,
        },
    };

    // Wrap in PROPVARIANT (VT_BLOB)
    let mut prop_variant = windows::Win32::System::Com::StructuredStorage::PROPVARIANT::default();
    unsafe {
        let inner = &mut prop_variant.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob.cbSize =
            std::mem::size_of::<AudioClientActivationParams>() as u32;
        inner.Anonymous.blob.pBlobData =
            &mut activation_params as *mut _ as *mut u8;
    }

    // Create completion handler
    let (tx, rx) = mpsc::channel();
    let handler: Audio::IActivateAudioInterfaceCompletionHandler =
        ActivationCompletionHandler {
            tx: std::sync::Mutex::new(Some(tx)),
        }
        .into();

    // Convert device path to wide string
    let device_path: Vec<u16> = VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let device_path_pcwstr = windows::core::PCWSTR(device_path.as_ptr());

    // Call ActivateAudioInterfaceAsync
    let _async_op = unsafe {
        Audio::ActivateAudioInterfaceAsync(
            device_path_pcwstr,
            &Audio::IAudioClient::IID,
            Some(&prop_variant),
            &handler,
        )
        .map_err(|e| {
            let description = format!(
                "ActivateAudioInterfaceAsync failed (process loopback for PID {}): {}",
                target_pid, e
            );
            BuildStreamError::from(BackendSpecificError { description })
        })?
    };

    // Wait for completion (with timeout)
    let audio_client = rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| {
            let description = format!(
                "ActivateAudioInterfaceAsync timed out for PID {}",
                target_pid
            );
            BuildStreamError::from(BackendSpecificError { description })
        })?
        .map_err(|e| {
            let description = format!(
                "ActivateAudioInterfaceAsync failed for PID {}: {}",
                target_pid, e
            );
            BuildStreamError::from(BackendSpecificError { description })
        })?;

    Ok(audio_client)
}

/// Resolve excluded app names/PIDs from a `StreamConfig` into target PIDs.
///
/// Returns a list of PIDs to exclude. The caller decides the capture strategy:
/// - 0 PIDs → classic loopback (capture all system audio)
/// - 1 PID → per-process EXCLUDE loopback (single API call)
/// - 2+ PIDs → audio subtraction (Total - INCLUDE_A - INCLUDE_B - ...)
fn resolve_excluded_pids(config: &StreamConfig) -> Vec<u32> {
    // Check explicit PIDs first
    if let Some(ref pids) = config.excluded_app_pids {
        if !pids.is_empty() {
            let raw: Vec<u32> = pids.iter().map(|&p| p as u32).collect();
            return process::deduplicate_pids_by_tree(&raw);
        }
    }

    // Resolve app names to PIDs
    if let Some(ref names) = config.excluded_app_names {
        if !names.is_empty() {
            let raw: Vec<u32> = process::find_pids_by_name_substrings(names)
                .into_iter()
                .map(|p| p.pid)
                .collect();
            return process::deduplicate_pids_by_tree(&raw);
        }
    }

    Vec::new()
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
