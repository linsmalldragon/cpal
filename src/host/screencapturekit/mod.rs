use std::time::Duration;
use std::{cell::RefCell, rc::Rc};

use crate::{
    device_description::DeviceDescriptionBuilder,
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BackendSpecificError, BuildStreamError, Data, DefaultStreamConfigError, DeviceDescription,
    DeviceId, DeviceIdError, DeviceNameError, DevicesError, HostId, InputCallbackInfo,
    OutputCallbackInfo, PauseStreamError, PlayStreamError, SampleFormat, SampleRate, StreamConfig,
    StreamError, StreamInstant, SupportedBufferSize, SupportedStreamConfig,
    SupportedStreamConfigRange, SupportedStreamConfigsError,
};

use block2::RcBlock;
use objc2::rc::{Allocated, Retained};
use objc2::{class, define_class, msg_send, ClassType, DefinedClass};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType, SCWindow,
};

/// Error type for updating content filter during streaming
#[derive(Debug, Clone)]
pub struct UpdateFilterError {
    pub description: String,
}

impl std::fmt::Display for UpdateFilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UpdateFilterError: {}", self.description)
    }
}

impl std::error::Error for UpdateFilterError {}

impl Capturer {
    pub fn new(inner: CapturerInner) -> Retained<Self> {
        unsafe {
            // Workaround: msg_send! infers Retained, but we need Allocated.
            // Both are transparent wrappers around NonNull.
            let ptr: *mut Self = msg_send![Self::class(), alloc];
            let alloc: Allocated<Self> = std::mem::transmute(ptr);
            // set_ivars returns PartialInit<Self>
            let partial = alloc.set_ivars(RefCell::new(inner));
            // Workaround: Transmute PartialInit back to Allocated or use it directly
            // If MsgSend fails for PartialInit, casting to Allocated might help
            let alloc_again: Allocated<Self> = std::mem::transmute(partial);
            let obj: Option<Retained<Self>> = msg_send![alloc_again, init];
            obj.expect("Capturer init failed")
        }
    }
}

pub use enumerate::{
    default_input_device, default_output_device, Devices, SupportedInputConfigs,
    SupportedOutputConfigs,
};

pub mod enumerate;

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
        // Assume screencapturekit is always available
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

#[derive(Clone)]
pub struct Device {
    display_id: u32,
    excluded_apps_pids: Vec<i32>,
    /// App names to exclude (matched at stream build time)
    excluded_app_names: Vec<String>,
}

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    fn name(&self) -> Result<String, DeviceNameError> {
        Ok(format!("Display {}", self.display_id))
    }

    fn description(&self) -> Result<DeviceDescription, DeviceNameError> {
        let name = format!("Display {}", self.display_id);
        Ok(DeviceDescriptionBuilder::new(name)
            .device_type(crate::device_description::DeviceType::ScreenCaptureKit)
            .interface_type(crate::device_description::InterfaceType::Unknown)
            .direction(crate::device_description::DeviceDirection::Input)
            .build())
    }

    fn id(&self) -> Result<DeviceId, DeviceIdError> {
        Ok(DeviceId(
            HostId::ScreenCaptureKit,
            self.display_id.to_string(),
        ))
    }

    fn supported_input_configs(
        &self,
    ) -> Result<Self::SupportedInputConfigs, SupportedStreamConfigsError> {
        Self::supported_input_configs(self)
    }

    fn supported_output_configs(
        &self,
    ) -> Result<Self::SupportedOutputConfigs, SupportedStreamConfigsError> {
        Self::supported_output_configs(self)
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        Self::default_input_config(self)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        Self::default_output_config(self)
    }

    fn build_input_stream_raw<D, E>(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        _timeout: Option<std::time::Duration>,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        Self::build_input_stream(self, config, sample_format, data_callback, error_callback)
    }

    fn build_output_stream_raw<D, E>(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        Self::build_output_stream(
            self,
            config,
            sample_format,
            data_callback,
            error_callback,
            timeout,
        )
    }
}

impl Device {
    pub fn new(display: Retained<SCDisplay>) -> Self {
        let display_id = unsafe { display.displayID() };
        Self {
            display_id,
            excluded_apps_pids: Vec::new(),
            excluded_app_names: Vec::new(),
        }
    }

    /// Set excluded apps by SCRunningApplication references (advanced API)
    pub fn set_excluded_apps(&mut self, apps: &[Retained<SCRunningApplication>]) {
        self.excluded_apps_pids = apps.iter().map(|a| unsafe { a.processID() }).collect();
    }

    /// Set excluded apps by name (simple API, recommended)
    ///
    /// App names are matched against running applications when building the stream.
    /// Uses a cached list of running apps for performance.
    ///
    /// # Example
    /// ```ignore
    /// device.set_excluded_app_names(&["QQ音乐", "微信"]);
    /// ```
    pub fn set_excluded_apps_pids(&mut self, pids: &[i32]) {
        self.excluded_apps_pids = pids.to_vec();
    }

    /// Set excluded apps by name (simple API, recommended)
    ///
    /// App names are matched against running applications when building the stream.
    /// Uses a cached list of running apps for performance.
    ///
    /// # Example
    /// ```ignore
    /// device.set_excluded_app_names(&["QQ音乐", "微信"]);
    /// ```
    pub fn set_excluded_app_names(&mut self, names: &[&str]) {
        self.excluded_app_names = names.iter().map(|s| s.to_string()).collect();
    }

    /// Get the list of excluded app names
    pub fn excluded_app_names(&self) -> &[String] {
        &self.excluded_app_names
    }

    fn supported_input_configs(
        &self,
    ) -> Result<SupportedInputConfigs, SupportedStreamConfigsError> {
        let channels = 2;
        let min_sample_rate: SampleRate = 48_000;
        let max_sample_rate: SampleRate = 48_000;
        let buffer_size = SupportedBufferSize::Unknown;
        let sample_format = SampleFormat::F32;
        let supported_configs = vec![SupportedStreamConfigRange {
            channels,
            min_sample_rate,
            max_sample_rate,
            buffer_size,
            sample_format,
        }];
        Ok(supported_configs.into_iter())
    }

    fn supported_output_configs(
        &self,
    ) -> Result<SupportedOutputConfigs, SupportedStreamConfigsError> {
        Ok(Vec::new().into_iter())
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        let config = Self::supported_input_configs(self)
            .expect("failed to get supported input configs")
            .next()
            .expect("no supported input configs")
            .with_max_sample_rate();
        Ok(config)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        Err(DefaultStreamConfigError::StreamTypeNotSupported)
    }

    fn build_input_stream<D, E>(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
    ) -> Result<Stream, BuildStreamError>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        use objc2::class;
        use objc2::rc::Allocated;

        let cfg = unsafe { SCStreamConfiguration::new() };
        unsafe {
            cfg.setCapturesAudio(true);
            cfg.setExcludesCurrentProcessAudio(false);
        }

        // Resolve display object from ID
        let display = enumerate::get_display_by_id(self.display_id).ok_or_else(|| {
            BuildStreamError::BackendSpecific {
                err: BackendSpecificError {
                    description: format!("Display {} not found", self.display_id),
                },
            }
        })?;

        let windows: Retained<NSArray<SCWindow>> = NSArray::new();

        // Resolve excluded app names/PIDs to SCRunningApplication objects
        let mut excluded_apps_refs: Vec<Retained<SCRunningApplication>> = Vec::new();

        // 1. Resolve by names associated with this device config
        if !self.excluded_app_names.is_empty() {
            let matched_apps = enumerate::find_apps_by_name_substrings(&self.excluded_app_names);
            excluded_apps_refs.extend(matched_apps);
        }

        // 2. Resolve by PIDs associated with this device config
        if !self.excluded_apps_pids.is_empty() {
            for pid in &self.excluded_apps_pids {
                if let Some(app) = enumerate::get_running_application_by_pid(*pid) {
                    excluded_apps_refs.push(app);
                }
            }
        }

        let filter: Retained<SCContentFilter> = if excluded_apps_refs.is_empty() {
            unsafe {
                let ptr: *mut SCContentFilter = msg_send![class!(SCContentFilter), alloc];
                let alloc: Allocated<SCContentFilter> = std::mem::transmute(ptr);
                SCContentFilter::initWithDisplay_excludingWindows(alloc, &display, &windows)
            }
        } else {
            // Deduplicate if needed, or just let SCContentFilter handle it (it likely handles dupes fine)
            let apps_refs: Vec<&SCRunningApplication> =
                excluded_apps_refs.iter().map(|a| &**a).collect();
            let excluded_apps_nsarray = NSArray::from_slice(&apps_refs);
            unsafe {
                let ptr: *mut SCContentFilter = msg_send![class!(SCContentFilter), alloc];
                let alloc: Allocated<SCContentFilter> = std::mem::transmute(ptr);
                SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                    alloc,
                    &display,
                    &excluded_apps_nsarray,
                    &windows,
                )
            }
        };

        let sc_stream = unsafe {
            let ptr: *mut SCStream = msg_send![class!(SCStream), alloc];
            let alloc: Allocated<SCStream> = std::mem::transmute(ptr);
            SCStream::initWithFilter_configuration_delegate(alloc, &filter, &cfg, None)
        };

        let inner = CapturerInner {
            current_data: vec![],
            config: config.clone(),
            sample_format,
            data_callback: Box::new(data_callback),
            error_callback: Box::new(error_callback),
        };

        let capturer = Capturer::new(inner);

        // Dispatch queue handling
        let label = std::ffi::CString::new("cpal.screencapturekit.queue").unwrap();
        let queue = unsafe { dispatch_queue_create(label.as_ptr(), std::ptr::null_mut()) };

        unsafe {
            let queue_obj: *mut objc2::runtime::AnyObject = queue as _;
            let queue_retained: Retained<objc2::runtime::AnyObject> =
                Retained::from_raw(queue_obj).unwrap();

            let mut error: *mut NSError = std::ptr::null_mut();
            let success: bool = msg_send![
                &sc_stream,
                addStreamOutput: &*capturer,
                type: SCStreamOutputType::Audio,
                sampleHandlerQueue: &*queue_retained,
                error: &mut error
            ];

            if !success {
                let err = if !error.is_null() {
                    Retained::retain(error)
                        .map(|e| format!("{:?}", e))
                        .unwrap_or_else(|| "Unknown error".to_string())
                } else {
                    "Unknown SCStream error".to_string()
                };
                return Err(BackendSpecificError { description: err }.into());
            }
        }

        Ok(Stream::new(StreamInner {
            _capturer: capturer,
            sc_stream,
            playing: false,
            display_id: self.display_id,
        }))
    }

    fn build_output_stream<D, E>(
        &self,
        _config: &StreamConfig,
        _sample_format: SampleFormat,
        _data_callback: D,
        _error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Stream, BuildStreamError>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        Err(BuildStreamError::StreamConfigNotSupported)
    }
}

struct StreamInner {
    // Keep capturer alive
    _capturer: Retained<Capturer>,
    sc_stream: Retained<SCStream>,
    playing: bool,
    // Store display_id for dynamic filter updates
    display_id: u32,
}

#[derive(Clone)]
pub struct Stream {
    inner: Rc<RefCell<StreamInner>>,
}

impl Stream {
    fn new(inner: StreamInner) -> Self {
        Self {
            inner: Rc::new(RefCell::new(inner)),
        }
    }

    /// Update excluded applications by PIDs during streaming (without interrupting capture)
    ///
    /// This dynamically updates the content filter to exclude apps with the given PIDs.
    /// The audio stream continues without interruption.
    ///
    /// # Example
    /// ```ignore
    /// // Exclude QQ Music (assume PID is 12345)
    /// stream.update_excluded_apps_by_pids(&[12345])?;
    ///
    /// // Clear all exclusions
    /// stream.update_excluded_apps_by_pids(&[])?;
    /// ```
    pub fn update_excluded_apps_by_pids(&self, pids: &[i32]) -> Result<(), UpdateFilterError> {
        let stream = self.inner.borrow();
        let display_id = stream.display_id;

        // Resolve excluded apps from PIDs
        let mut excluded_apps_refs: Vec<Retained<SCRunningApplication>> = Vec::new();
        for pid in pids {
            if let Some(app) = enumerate::get_running_application_by_pid(*pid) {
                excluded_apps_refs.push(app);
            }
        }

        self.update_content_filter_internal(display_id, &excluded_apps_refs)
    }

    /// Update excluded applications by name substrings during streaming (without interrupting capture)
    ///
    /// This dynamically updates the content filter to exclude apps whose names contain
    /// any of the given substrings. The audio stream continues without interruption.
    ///
    /// # Example
    /// ```ignore
    /// // Exclude QQ Music and WeChat
    /// stream.update_excluded_apps_by_names(&["QQ音乐", "微信"])?;
    ///
    /// // Clear all exclusions
    /// stream.update_excluded_apps_by_names(&[])?;
    /// ```
    pub fn update_excluded_apps_by_names(&self, names: &[&str]) -> Result<(), UpdateFilterError> {
        let stream = self.inner.borrow();
        let display_id = stream.display_id;

        // Resolve excluded apps from names
        let names_vec: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        let excluded_apps_refs = enumerate::find_apps_by_name_substrings(&names_vec);

        self.update_content_filter_internal(display_id, &excluded_apps_refs)
    }

    /// Update excluded applications by bundle identifiers during streaming (without interrupting capture)
    ///
    /// This dynamically updates the content filter to exclude apps with the given bundle IDs.
    /// The audio stream continues without interruption.
    ///
    /// Bundle IDs are like "com.tencent.QQMusicMac", "com.apple.Safari", etc.
    /// You can find an app's bundle ID using: `mdls -name kMDItemCFBundleIdentifier /Applications/AppName.app`
    ///
    /// # Example
    /// ```ignore
    /// // Exclude QQ Music by bundle ID
    /// stream.update_excluded_apps_by_bundle_ids(&["com.tencent.QQMusicMac"])?;
    ///
    /// // Clear all exclusions
    /// stream.update_excluded_apps_by_bundle_ids(&[])?;
    /// ```
    pub fn update_excluded_apps_by_bundle_ids(
        &self,
        bundle_ids: &[&str],
    ) -> Result<(), UpdateFilterError> {
        let stream = self.inner.borrow();
        let display_id = stream.display_id;

        // Resolve excluded apps from bundle IDs
        let bundle_ids_vec: Vec<String> = bundle_ids.iter().map(|s| s.to_string()).collect();
        let excluded_apps_refs = enumerate::find_apps_by_bundle_ids(&bundle_ids_vec);

        self.update_content_filter_internal(display_id, &excluded_apps_refs)
    }

    /// Internal method to update the content filter
    fn update_content_filter_internal(
        &self,
        display_id: u32,
        excluded_apps: &[Retained<SCRunningApplication>],
    ) -> Result<(), UpdateFilterError> {
        // Get the display
        let display =
            enumerate::get_display_by_id(display_id).ok_or_else(|| UpdateFilterError {
                description: format!("Display {} not found", display_id),
            })?;

        let windows: Retained<NSArray<SCWindow>> = NSArray::new();

        // Build new content filter
        let filter: Retained<SCContentFilter> = if excluded_apps.is_empty() {
            unsafe {
                let ptr: *mut SCContentFilter = msg_send![class!(SCContentFilter), alloc];
                let alloc: Allocated<SCContentFilter> = std::mem::transmute(ptr);
                SCContentFilter::initWithDisplay_excludingWindows(alloc, &display, &windows)
            }
        } else {
            let apps_refs: Vec<&SCRunningApplication> =
                excluded_apps.iter().map(|a| &**a).collect();
            let excluded_apps_nsarray = NSArray::from_slice(&apps_refs);
            unsafe {
                let ptr: *mut SCContentFilter = msg_send![class!(SCContentFilter), alloc];
                let alloc: Allocated<SCContentFilter> = std::mem::transmute(ptr);
                SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                    alloc,
                    &display,
                    &excluded_apps_nsarray,
                    &windows,
                )
            }
        };

        // Update the stream's content filter
        let (tx, rx) = std::sync::mpsc::channel();

        let handler = RcBlock::new(move |error: *mut NSError| {
            if !error.is_null() {
                let err = unsafe { Retained::retain(error) };
                let _ = tx.send(Err(UpdateFilterError {
                    description: format!("{:?}", err),
                }));
            } else {
                let _ = tx.send(Ok(()));
            }
        });

        let stream = self.inner.borrow();
        unsafe {
            stream
                .sc_stream
                .updateContentFilter_completionHandler(&filter, Some(&handler));
        }

        // Wait for completion with timeout
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(result) => result,
            Err(_) => Err(UpdateFilterError {
                description: "Timeout waiting for updateContentFilter".to_string(),
            }),
        }
    }

    /// Refresh the applications cache and return the new list
    ///
    /// Call this if you need to detect newly launched applications.
    /// The returned list can be used to find PIDs for `update_excluded_apps_by_pids`.
    pub fn refresh_applications_cache(
    ) -> Result<Vec<Retained<SCRunningApplication>>, UpdateFilterError> {
        enumerate::refresh_applications_cache().map_err(|e| UpdateFilterError {
            description: format!("{:?}", e),
        })?;
        enumerate::get_applications_cached().map_err(|e| UpdateFilterError {
            description: format!("{:?}", e),
        })
    }
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), PlayStreamError> {
        let mut stream = self.inner.borrow_mut();
        if !stream.playing {
            let (tx, rx) = std::sync::mpsc::channel();

            // SCStream uses completion handler blocks
            let handler = RcBlock::new(move |error: *mut NSError| {
                if !error.is_null() {
                    let err = unsafe { Retained::retain(error) };
                    tx.send(Err(BackendSpecificError {
                        description: format!("{:?}", err),
                    }))
                    .unwrap();
                } else {
                    tx.send(Ok(())).unwrap();
                }
            });

            unsafe {
                stream
                    .sc_stream
                    .startCaptureWithCompletionHandler(Some(&handler));
            }

            rx.recv().unwrap()?;
            stream.playing = true;
        }
        Ok(())
    }

    fn pause(&self) -> Result<(), PauseStreamError> {
        let mut stream = self.inner.borrow_mut();
        if stream.playing {
            let (tx, rx) = std::sync::mpsc::channel();

            let handler = RcBlock::new(move |error: *mut NSError| {
                if !error.is_null() {
                    let err = unsafe { Retained::retain(error) };
                    tx.send(Err(BackendSpecificError {
                        description: format!("{:?}", err),
                    }))
                    .unwrap();
                } else {
                    tx.send(Ok(())).unwrap();
                }
            });

            unsafe {
                stream
                    .sc_stream
                    .stopCaptureWithCompletionHandler(Some(&handler));
            }

            rx.recv().unwrap()?;
            stream.playing = false;
        }
        Ok(())
    }
}

#[allow(dead_code)]
pub struct CapturerInner {
    current_data: Vec<f32>,
    config: StreamConfig,
    sample_format: SampleFormat,
    data_callback: Box<dyn FnMut(&Data, &InputCallbackInfo) + Send + 'static>,
    error_callback: Box<dyn FnMut(StreamError) + Send + 'static>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "CpalScreencaptureKitCapturer"]
    #[ivars = RefCell<CapturerInner>]
    pub struct Capturer;

    unsafe impl NSObjectProtocol for Capturer {}

    unsafe impl SCStreamOutput for Capturer {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output_sample_buffer_of_type(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Audio {
                let mut inner = self.ivars().borrow_mut();
                inner.handle_audio(sample_buffer);
            }
        }
    }
);

#[allow(unused)]
fn frames_to_duration(frames: usize, rate: crate::SampleRate) -> std::time::Duration {
    let secsf = frames as f64 / (rate as f64);
    let secs = secsf as u64;
    let nanos = ((secsf - secs as f64) * 1_000_000_000.0) as u32;
    std::time::Duration::new(secs, nanos)
}

// Manual FFI bindings
#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMSampleBufferGetNumSamples(sbuf: &CMSampleBuffer) -> std::ffi::c_long;
    fn CMSampleBufferGetPresentationTimeStamp(sbuf: &CMSampleBuffer) -> CMTime;
    fn CMSampleBufferGetDataBuffer(sbuf: &CMSampleBuffer) -> *mut std::ffi::c_void;
    fn CMBlockBufferGetDataPointer(
        theBuffer: *mut std::ffi::c_void,
        offset: usize,
        lengthAtOffsetOut: *mut usize,
        totalLengthOut: *mut usize,
        dataPointerOut: *mut *mut u8,
    ) -> i32;
}

#[link(name = "System", kind = "dylib")]
extern "C" {
    fn dispatch_queue_create(
        label: *const i8,
        attr: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
}

impl CapturerInner {
    fn handle_audio(&mut self, sample_buf: &CMSampleBuffer) {
        unsafe {
            let num_samples = CMSampleBufferGetNumSamples(sample_buf) as usize;
            if num_samples == 0 {
                return;
            }

            let timestamp = CMSampleBufferGetPresentationTimeStamp(sample_buf);
            let instant = host_time_to_stream_instant(timestamp);

            let block_buffer = CMSampleBufferGetDataBuffer(sample_buf);
            if block_buffer.is_null() {
                return;
            }

            let mut length_at_offset = 0;
            let mut total_length = 0;
            let mut data_ptr: *mut u8 = std::ptr::null_mut();

            let status = CMBlockBufferGetDataPointer(
                block_buffer,
                0,
                &mut length_at_offset,
                &mut total_length,
                &mut data_ptr,
            );

            if status != 0 {
                return;
            }
            let float_ptr = data_ptr as *const f32;
            let floats_len = total_length / 4;
            let samples = std::slice::from_raw_parts(float_ptr, floats_len);

            self.current_data.resize(floats_len, 0.0);
            self.current_data.copy_from_slice(samples);

            // Callback
            let callback_info = InputCallbackInfo {
                timestamp: crate::InputStreamTimestamp {
                    callback: instant,
                    capture: instant,
                },
            };

            let cpal_data = Data::from_parts(
                self.current_data.as_mut_ptr() as *mut _,
                self.current_data.len(),
                SampleFormat::F32,
            );
            (self.data_callback)(&cpal_data, &callback_info);
        }
    }
}

fn host_time_to_stream_instant(cm_time: CMTime) -> StreamInstant {
    let secs = cm_time.value / cm_time.timescale as i64;
    let subsec_nanos =
        (cm_time.value % cm_time.timescale as i64) * 1_000_000_000 / cm_time.timescale as i64;
    StreamInstant::new(secs, subsec_nanos as u32)
}
