use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
use objc2::runtime::ProtocolObject;
use objc2::{class, define_class, msg_send, ClassType, DefinedClass};
use objc2_core_graphics::CGDirectDisplayID;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCRunningApplication, SCStream, SCStreamConfiguration, SCStreamDelegate,
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
    pub fn new(inner: CapturerInner, stream_stopped: Arc<AtomicBool>) -> Retained<Self> {
        let ivars = CapturerIvars {
            inner: RefCell::new(inner),
            stream_stopped,
        };
        unsafe {
            // Workaround: msg_send! infers Retained, but we need Allocated.
            // Both are transparent wrappers around NonNull.
            let ptr: *mut Self = msg_send![Self::class(), alloc];
            let alloc: Allocated<Self> = std::mem::transmute(ptr);
            // set_ivars returns PartialInit<Self>
            let partial = alloc.set_ivars(ivars);
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

// CoreGraphics FFI for display identification
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGDisplaySerialNumber(display: CGDirectDisplayID) -> u32;
}

/// Compute a unique display identifier compatible with xcap's `unique_key()` logic.
/// Priority: serial number → CGDirectDisplayID (fallback)
fn get_display_unique_id(display_id: CGDirectDisplayID) -> String {
    // 1. Try serial number (hardware attribute, most reliable and unique across displays)
    let serial = unsafe { CGDisplaySerialNumber(display_id) };
    if serial != 0 {
        return serial.to_string();
    }

    // 2. Fallback: CGDirectDisplayID (may be small numbers like 1, 2, 3)
    display_id.to_string()
}

/// Get the computer's hostname for use in device display names.
/// Returns something like "MacBook-Pro" or "linxiaolong-MacBookPro".
/// Falls back to "Mac" if gethostname fails.
fn get_computer_name() -> String {
    extern "C" {
        fn gethostname(name: *mut std::ffi::c_char, len: usize) -> i32;
    }
    let mut buf = [0u8; 256];
    let ret = unsafe { gethostname(buf.as_mut_ptr() as *mut std::ffi::c_char, buf.len()) };
    if ret == 0 {
        let name = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr() as *const std::ffi::c_char) };
        let name_str = name.to_string_lossy().to_string();
        // Remove ".local" suffix if present (macOS often appends it)
        name_str
            .strip_suffix(".local")
            .unwrap_or(&name_str)
            .to_string()
    } else {
        "Mac".to_string()
    }
}

#[derive(Clone)]
pub struct Device {
    display_id: u32,
    /// Unique identifier: serial number, UUID, or displayID (fallback)
    unique_id: String,
}

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    fn name(&self) -> Result<String, DeviceNameError> {
        let computer_name = get_computer_name();
        Ok(format!("{} Audio", computer_name))
    }

    fn description(&self) -> Result<DeviceDescription, DeviceNameError> {
        let computer_name = get_computer_name();
        let name = format!("{} Audio", computer_name);
        Ok(DeviceDescriptionBuilder::new(name)
            .device_type(crate::device_description::DeviceType::ScreenCaptureKit)
            .interface_type(crate::device_description::InterfaceType::Unknown)
            .direction(crate::device_description::DeviceDirection::Input)
            .build())
    }

    fn id(&self) -> Result<DeviceId, DeviceIdError> {
        Ok(DeviceId(HostId::ScreenCaptureKit, self.unique_id.clone()))
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
    pub fn new(display_id: u32) -> Self {
        let unique_id = get_display_unique_id(display_id);
        Self {
            display_id,
            unique_id,
        }
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

        // 1. Resolve by names from StreamConfig
        if let Some(ref excluded_names) = config.excluded_app_names {
            if !excluded_names.is_empty() {
                let matched_apps = enumerate::find_apps_by_name_substrings(excluded_names);
                excluded_apps_refs.extend(matched_apps);
            }
        }

        // 2. Resolve by PIDs from StreamConfig
        if let Some(ref excluded_pids) = config.excluded_app_pids {
            for pid in excluded_pids {
                if let Some(app) = enumerate::get_running_application_by_pid(*pid) {
                    excluded_apps_refs.push(app);
                }
            }
        }

        // 3. Resolve by bundle IDs from StreamConfig
        if let Some(ref excluded_bundle_ids) = config.excluded_app_bundle_ids {
            if !excluded_bundle_ids.is_empty() {
                let matched_apps = enumerate::find_apps_by_bundle_ids(excluded_bundle_ids);
                excluded_apps_refs.extend(matched_apps);
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

        let stream_stopped = Arc::new(AtomicBool::new(false));

        let inner = CapturerInner {
            current_data: vec![],
            config: config.clone(),
            sample_format,
            data_callback: Box::new(data_callback),
            error_callback: Box::new(error_callback),
        };

        let capturer = Capturer::new(inner, stream_stopped.clone());

        // Pass capturer as delegate to receive stream:didStopWithError: callbacks
        let delegate = ProtocolObject::from_ref(&*capturer);
        let sc_stream = unsafe {
            let ptr: *mut SCStream = msg_send![class!(SCStream), alloc];
            let alloc: Allocated<SCStream> = std::mem::transmute(ptr);
            SCStream::initWithFilter_configuration_delegate(alloc, &filter, &cfg, Some(delegate))
        };

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

        // Extract exclusion config for auto-refresh
        let excluded_names = config.excluded_app_names.clone().unwrap_or_default();
        let excluded_bundle_ids = config.excluded_app_bundle_ids.clone().unwrap_or_default();

        let exclusion_refresh_needed = Arc::new(AtomicBool::new(false));

        // Register NSWorkspace observer to detect when excluded apps launch
        let app_launch_observer = if !excluded_names.is_empty() || !excluded_bundle_ids.is_empty() {
            Some(register_app_launch_observer(
                excluded_names.clone(),
                excluded_bundle_ids.clone(),
                exclusion_refresh_needed.clone(),
            ))
        } else {
            None
        };

        Ok(Stream::new(
            StreamInner {
                _capturer: capturer,
                sc_stream,
                playing: false,
                display_id: self.display_id,
                stream_stopped,
                excluded_app_names: excluded_names,
                excluded_app_bundle_ids: excluded_bundle_ids,
                app_launch_observer,
            },
            exclusion_refresh_needed,
        ))
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
    // Flag set when SCStreamDelegate reports stream stopped
    stream_stopped: Arc<AtomicBool>,
    // Exclusion config for auto-refresh when excluded apps launch after stream creation
    excluded_app_names: Vec<String>,
    excluded_app_bundle_ids: Vec<String>,
    // NSWorkspace observer handle for cleanup on drop (Drop impl on AppLaunchObserver removes the observer)
    #[allow(dead_code)]
    app_launch_observer: Option<AppLaunchObserver>,
}

/// Holds the NSNotificationCenter + observer token so we can removeObserver: on Drop
struct AppLaunchObserver {
    center: *mut objc2::runtime::AnyObject,
    observer: *mut objc2::runtime::AnyObject,
}

unsafe impl Send for AppLaunchObserver {}
unsafe impl Sync for AppLaunchObserver {}

impl Drop for AppLaunchObserver {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![self.center, removeObserver: self.observer];
        }
    }
}

#[derive(Clone)]
pub struct Stream {
    inner: Rc<RefCell<StreamInner>>,
    /// Direct access to stream_stopped flag — no RefCell borrow needed
    stream_stopped: Arc<AtomicBool>,
    /// Set by NSWorkspace observer when a matching excluded app launches
    exclusion_refresh_needed: Arc<AtomicBool>,
}

impl Stream {
    fn new(inner: StreamInner, exclusion_refresh_needed: Arc<AtomicBool>) -> Self {
        let stream_stopped = inner.stream_stopped.clone();
        Self {
            inner: Rc::new(RefCell::new(inner)),
            stream_stopped,
            exclusion_refresh_needed,
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

    /// Update excluded applications by both name substrings and bundle identifiers in a single
    /// filter update (without interrupting capture)
    ///
    /// This resolves apps from both names and bundle IDs, merges them, and applies a single
    /// SCContentFilter update. This avoids the problem where calling `update_excluded_apps_by_names`
    /// and `update_excluded_apps_by_bundle_ids` separately would cause the second call to replace
    /// the first.
    pub fn update_excluded_apps(&self, names: &[&str], bundle_ids: &[&str]) -> Result<(), UpdateFilterError> {
        let stream = self.inner.borrow();
        let display_id = stream.display_id;

        let mut excluded_apps: Vec<Retained<SCRunningApplication>> = Vec::new();

        if !names.is_empty() {
            let names_vec: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            excluded_apps.extend(enumerate::find_apps_by_name_substrings(&names_vec));
        }

        if !bundle_ids.is_empty() {
            let bundle_ids_vec: Vec<String> = bundle_ids.iter().map(|s| s.to_string()).collect();
            excluded_apps.extend(enumerate::find_apps_by_bundle_ids(&bundle_ids_vec));
        }

        self.update_content_filter_internal(display_id, &excluded_apps)
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

    /// Check if excluded apps have launched since stream creation and refresh the content filter.
    ///
    /// An NSWorkspace observer sets the internal flag when a matching app launches.
    /// Call this periodically from a polling loop to apply the updated exclusions.
    /// Returns `Ok(true)` if the filter was refreshed, `Ok(false)` if no refresh was needed.
    pub fn auto_refresh_exclusions(&self) -> Result<bool, UpdateFilterError> {
        if !self.exclusion_refresh_needed.swap(false, Ordering::Acquire) {
            return Ok(false);
        }

        // Invalidate app cache to pick up newly launched processes
        enumerate::invalidate_applications_cache();

        let stream = self.inner.borrow();
        let display_id = stream.display_id;
        let names = stream.excluded_app_names.clone();
        let bundle_ids = stream.excluded_app_bundle_ids.clone();
        drop(stream);

        if names.is_empty() && bundle_ids.is_empty() {
            return Ok(false);
        }

        let mut excluded_apps: Vec<Retained<SCRunningApplication>> = Vec::new();
        if !names.is_empty() {
            excluded_apps.extend(enumerate::find_apps_by_name_substrings(&names));
        }
        if !bundle_ids.is_empty() {
            excluded_apps.extend(enumerate::find_apps_by_bundle_ids(&bundle_ids));
        }

        self.update_content_filter_internal(display_id, &excluded_apps)?;
        Ok(true)
    }

    /// Check if the stream has been stopped by the system (e.g., display disconnected)
    ///
    /// This reads the Arc<AtomicBool> directly — no RefCell borrow needed,
    /// so it is always safe to call regardless of what other borrows are active.
    pub fn is_stream_stopped(&self) -> bool {
        self.stream_stopped.load(Ordering::Acquire)
    }
}

/// Register an NSWorkspace observer that sets the flag when a matching excluded app launches.
///
/// Uses `NSWorkspaceDidLaunchApplicationNotification`. Returns an `AppLaunchObserver` that
/// removes the observer on Drop, preventing observer accumulation across stream recreations.
fn register_app_launch_observer(
    excluded_names: Vec<String>,
    excluded_bundle_ids: Vec<String>,
    flag: Arc<AtomicBool>,
) -> AppLaunchObserver {
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSNotificationName;
    use std::ptr::NonNull;

    unsafe {
        // NSWorkspace.sharedWorkspace
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        // workspace.notificationCenter
        let center: *mut AnyObject = msg_send![workspace, notificationCenter];

        let name = NSNotificationName::from_str("NSWorkspaceDidLaunchApplicationNotification");

        let block = RcBlock::new(
            move |notification: NonNull<objc2_foundation::NSNotification>| {
                let matched = extract_and_check_launched_app(
                    notification.as_ref(),
                    &excluded_names,
                    &excluded_bundle_ids,
                );
                if matched {
                    flag.store(true, Ordering::Release);
                }
            },
        );

        let observer: *mut AnyObject = msg_send![
            center,
            addObserverForName: &*name,
            object: std::ptr::null::<AnyObject>(),
            queue: std::ptr::null::<AnyObject>(),
            usingBlock: &*block
        ];
        // ObjC copies the block internally, so RcBlock can drop safely here

        AppLaunchObserver { center, observer }
    }
}

/// Extract launched app info from NSWorkspaceDidLaunchApplicationNotification and check exclusion match.
unsafe fn extract_and_check_launched_app(
    notification: &objc2_foundation::NSNotification,
    excluded_names: &[String],
    excluded_bundle_ids: &[String],
) -> bool {
    use objc2::runtime::AnyObject;
    use objc2_foundation::NSString;

    // notification.userInfo -> NSDictionary
    let user_info: *mut AnyObject = msg_send![notification, userInfo];
    if user_info.is_null() {
        return false;
    }

    // userInfo["NSWorkspaceApplicationKey"] -> NSRunningApplication
    let key = NSString::from_str("NSWorkspaceApplicationKey");
    let app: *mut AnyObject = msg_send![user_info, objectForKey: &*key];
    if app.is_null() {
        return false;
    }

    // Check bundle ID (exact match)
    if !excluded_bundle_ids.is_empty() {
        let bundle_id: *mut NSString = msg_send![app, bundleIdentifier];
        if !bundle_id.is_null() {
            let bundle_id_str = (*bundle_id).to_string();
            if excluded_bundle_ids.iter().any(|bid| bid == &bundle_id_str) {
                return true;
            }
        }
    }

    // Check app name (substring match)
    if !excluded_names.is_empty() {
        let app_name: *mut NSString = msg_send![app, localizedName];
        if !app_name.is_null() {
            let app_name_str = (*app_name).to_string();
            if excluded_names
                .iter()
                .any(|sub| app_name_str.contains(sub.as_str()))
            {
                return true;
            }
        }
    }

    false
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
                    if let Some(err) = err {
                        // Error code -3808 means stream is already stopped
                        // This can happen if ScreenCaptureKit stopped the stream internally
                        let error_code = err.code();
                        tx.send(Err((
                            error_code,
                            BackendSpecificError {
                                description: format!("{:?}", err),
                            },
                        )))
                        .unwrap();
                    } else {
                        tx.send(Err((
                            0,
                            BackendSpecificError {
                                description: "Unknown error (null NSError)".to_string(),
                            },
                        )))
                        .unwrap();
                    }
                } else {
                    tx.send(Ok(())).unwrap();
                }
            });

            unsafe {
                stream
                    .sc_stream
                    .stopCaptureWithCompletionHandler(Some(&handler));
            }

            match rx.recv().unwrap() {
                Ok(()) => {}
                Err((error_code, err)) => {
                    // Error code -3808: stream is already stopped or doesn't exist
                    // This is not a fatal error, just means the stream was already stopped
                    if error_code != -3808 {
                        return Err(err.into());
                    }
                    // Stream was already stopped, that's fine
                }
            }
            stream.playing = false;
        }
        Ok(())
    }
}

/// Ivars for the Objective-C Capturer class.
/// `stream_stopped` lives OUTSIDE the RefCell so `didStopWithError` can set it
/// without borrowing RefCell — completely eliminating the borrow conflict race condition.
pub struct CapturerIvars {
    inner: RefCell<CapturerInner>,
    stream_stopped: Arc<AtomicBool>,
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
    #[ivars = CapturerIvars]
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
                let mut inner = self.ivars().inner.borrow_mut();
                inner.handle_audio(sample_buffer);
            }
        }
    }

    unsafe impl SCStreamDelegate for Capturer {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            // 1. Set stream_stopped flag — directly on Arc<AtomicBool>, NO RefCell borrow.
            //    This is always safe regardless of concurrent audio callbacks.
            self.ivars().stream_stopped.store(true, Ordering::Release);

            // 2. Log Apple error details (no RefCell borrow needed)
            eprintln!("[cpal-sck] SCStream didStopWithError: {:?}", error);

            // 3. Try to call error_callback if RefCell is available.
            //    If audio callback holds the borrow, skip — the flag is already set
            //    and the pickup thread will detect it via polling.
            if let Ok(mut inner) = self.ivars().inner.try_borrow_mut() {
                (inner.error_callback)(StreamError::DeviceNotAvailable);
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
    fn CMSampleBufferGetFormatDescription(sbuf: &CMSampleBuffer) -> *const std::ffi::c_void;
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn CMAudioFormatDescriptionGetStreamBasicDescription(
        desc: *const std::ffi::c_void,
    ) -> *const AudioStreamBasicDescription;
}

/// AudioStreamBasicDescription from CoreAudio
#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(non_snake_case)]
struct AudioStreamBasicDescription {
    mSampleRate: f64,
    mFormatID: u32,
    mFormatFlags: u32,
    mBytesPerPacket: u32,
    mFramesPerPacket: u32,
    mBytesPerFrame: u32,
    mChannelsPerFrame: u32,
    mBitsPerChannel: u32,
    mReserved: u32,
}

// Audio format flags
#[allow(non_upper_case_globals)]
const kAudioFormatFlagIsFloat: u32 = 1 << 0;
#[allow(non_upper_case_globals)]
const kAudioFormatFlagIsNonInterleaved: u32 = 1 << 5;

// Global flag to print format info only once (across all threads)
static FORMAT_PRINTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

            // Get format description to check if non-interleaved
            let format_desc = CMSampleBufferGetFormatDescription(sample_buf);
            let is_non_interleaved = if !format_desc.is_null() {
                let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(format_desc);
                if !asbd.is_null() {
                    let asbd = &*asbd;
                    // Print format info only once (for debugging)
                    if !FORMAT_PRINTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        eprintln!("=== ScreenCaptureKit Audio Format ===");
                        eprintln!("  Sample Rate: {} Hz", asbd.mSampleRate);
                        eprintln!("  Channels: {}", asbd.mChannelsPerFrame);
                        eprintln!("  Bits per Channel: {}", asbd.mBitsPerChannel);
                        eprintln!("  Bytes per Frame: {}", asbd.mBytesPerFrame);
                        eprintln!("  Bytes per Packet: {}", asbd.mBytesPerPacket);
                        eprintln!("  Frames per Packet: {}", asbd.mFramesPerPacket);
                        eprintln!("  Format Flags: 0x{:08x}", asbd.mFormatFlags);
                        eprintln!(
                            "    Is Float: {}",
                            (asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0
                        );
                        eprintln!(
                            "    Is Non-Interleaved: {}",
                            (asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0
                        );
                        eprintln!("======================================");
                    }
                    (asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0
                } else {
                    false
                }
            } else {
                false
            };

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

            // Handle non-interleaved (planar) format by converting to interleaved
            if is_non_interleaved && self.config.channels == 2 {
                // Non-interleaved: [L0, L1, ..., Ln] [R0, R1, ..., Rn]
                // Need to convert to interleaved: [L0, R0, L1, R1, ..., Ln, Rn]
                let frames = floats_len / 2;
                self.current_data.resize(floats_len, 0.0);
                let left_channel = &samples[..frames];
                let right_channel = &samples[frames..];
                for i in 0..frames {
                    self.current_data[i * 2] = left_channel[i];
                    self.current_data[i * 2 + 1] = right_channel[i];
                }
            } else {
                self.current_data.resize(floats_len, 0.0);
                self.current_data.copy_from_slice(samples);
            }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{DeviceTrait, HostTrait};

    #[test]
    fn test_screencapturekit_device_ids_use_unique_display_identifier() {
        let host = Host::new().expect("Failed to create SCK host");
        let devices: Vec<Device> = host
            .devices()
            .expect("Failed to enumerate devices")
            .collect();

        println!("Found {} SCK display devices:", devices.len());
        assert!(!devices.is_empty(), "Should find at least one display");

        for device in &devices {
            let id = device.id().expect("Failed to get device ID");
            let name = device.name().expect("Failed to get device name");
            let id_str = id.to_string();

            println!(
                "  Device: {} | ID: {} | display_id(CGDirectDisplayID): {}",
                name, id_str, device.display_id
            );

            // Verify format: "screencapturekit:<unique_id>"
            assert!(
                id_str.starts_with("screencapturekit:"),
                "ID should start with 'screencapturekit:': {}",
                id_str
            );

            let unique_part = id_str.strip_prefix("screencapturekit:").unwrap();

            // The unique_id should NOT be a small index like "1", "2", "3"
            // unless CGDirectDisplayID is also the fallback (no serial/UUID available)
            // In that case, verify it matches the display_id
            println!(
                "  unique_id: {} | is_serial_or_uuid: {}",
                unique_part,
                unique_part.len() > 3
            );

            // Verify unique_id is non-empty
            assert!(!unique_part.is_empty(), "unique_id should not be empty");

            // Verify name uses unique_id
            assert!(
                name.contains(unique_part),
                "Name '{}' should contain unique_id '{}'",
                name,
                unique_part
            );
        }

        // Verify all IDs are unique
        let ids: Vec<String> = devices
            .iter()
            .map(|d| d.id().unwrap().to_string())
            .collect();
        let mut unique_ids = ids.clone();
        unique_ids.sort();
        unique_ids.dedup();
        assert_eq!(
            ids.len(),
            unique_ids.len(),
            "All device IDs should be unique: {:?}",
            ids
        );
    }

    #[test]
    fn test_get_display_unique_id_consistency() {
        // Call get_display_unique_id twice for the same display_id to verify consistency
        let host = Host::new().expect("Failed to create SCK host");
        let devices: Vec<Device> = host
            .devices()
            .expect("Failed to enumerate devices")
            .collect();

        for device in &devices {
            let id1 = get_display_unique_id(device.display_id);
            let id2 = get_display_unique_id(device.display_id);
            assert_eq!(
                id1, id2,
                "get_display_unique_id should return consistent results for display_id {}",
                device.display_id
            );
            println!(
                "display_id {} -> unique_id {} (consistent ✓)",
                device.display_id, id1
            );
        }
    }
}
