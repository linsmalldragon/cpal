use std::cell::RefCell;
use std::sync::mpsc::channel;
use std::time::Duration;
use std::vec::IntoIter as VecIntoIter;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{SCDisplay, SCRunningApplication, SCShareableContent};

use crate::{BackendSpecificError, DevicesError, SupportedStreamConfigRange};

use super::Device;

// ============================================================================
// Thread-local cache for displays and running applications
// ============================================================================
//
// 为什么使用 thread_local! 而不是 static Mutex:
// 1. Retained<SCShareableContent> 不是 Send + Sync，无法跨线程共享
// 2. thread_local! 实现线程本地缓存，每个线程有独立的缓存
// 3. 完全安全，不需要 unsafe impl Send + Sync
//
// 性能特点:
// - 单线程应用：完美，第一次后所有调用都很快（~1-2µs）
// - 多线程应用：每个线程首次调用需要 ~90ms，后续调用都很快
// - 缓存永不过期，直到线程结束自动清理
//
thread_local! {
    static DISPLAY_CACHE: RefCell<Option<Vec<Retained<SCDisplay>>>> = const { RefCell::new(None) };
    static APPS_CACHE: RefCell<Option<Vec<Retained<SCRunningApplication>>>> = const { RefCell::new(None) };
}

/// Fetch displays from macOS (blocking call ~90ms)
fn fetch_displays() -> Result<Vec<Retained<SCDisplay>>, DevicesError> {
    let (tx, rx) = channel();

    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            if !error.is_null() {
                let err = unsafe { Retained::retain(error) };
                let _ = tx.send(Err(BackendSpecificError {
                    description: format!("{:?}", err),
                }));
            } else if !content.is_null() {
                let content = unsafe { Retained::retain(content).unwrap() };
                let displays = unsafe { content.displays() };
                let display_vec: Vec<Retained<SCDisplay>> = displays.iter().collect();
                let _ = tx.send(Ok(display_vec));
            } else {
                let _ = tx.send(Err(BackendSpecificError {
                    description: "Unknown error: content and error are both null".to_string(),
                }));
            }
        },
    );

    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&handler);
    }

    // Add timeout for stability
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(result) => result.map_err(|e| e.into()),
        Err(_) => Err(BackendSpecificError {
            description: "Timeout waiting for SCShareableContent".to_string(),
        }
        .into()),
    }
}

/// Fetch running applications from macOS (blocking call ~90ms)
fn fetch_applications() -> Result<Vec<Retained<SCRunningApplication>>, DevicesError> {
    let (tx, rx) = channel();

    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            if !error.is_null() {
                let err = unsafe { Retained::retain(error) };
                let _ = tx.send(Err(BackendSpecificError {
                    description: format!("{:?}", err),
                }));
            } else if !content.is_null() {
                let content = unsafe { Retained::retain(content).unwrap() };
                let apps = unsafe { content.applications() };
                let apps_vec: Vec<Retained<SCRunningApplication>> = apps.iter().collect();
                let _ = tx.send(Ok(apps_vec));
            } else {
                let _ = tx.send(Err(BackendSpecificError {
                    description: "Unknown error: content and error are both null".to_string(),
                }));
            }
        },
    );

    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&handler);
    }

    // Add timeout for stability
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(result) => result.map_err(|e| e.into()),
        Err(_) => Err(BackendSpecificError {
            description: "Timeout waiting for SCShareableContent".to_string(),
        }
        .into()),
    }
}

/// Get displays with thread-local caching (fast on cache hit)
fn get_displays_cached() -> Result<Vec<Retained<SCDisplay>>, DevicesError> {
    // Check thread-local cache first
    let cached = DISPLAY_CACHE.with(|cache| cache.borrow().clone());

    if let Some(displays) = cached {
        return Ok(displays);
    }

    // Cache miss - fetch new content
    let displays = fetch_displays()?;

    // Update thread-local cache
    DISPLAY_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(displays.clone());
    });

    Ok(displays)
}

/// Get running applications with thread-local caching (fast on cache hit)
/// This is called internally when building a stream with app name exclusions
pub(crate) fn get_applications_cached() -> Result<Vec<Retained<SCRunningApplication>>, DevicesError>
{
    // Check thread-local cache first
    let cached = APPS_CACHE.with(|cache| cache.borrow().clone());

    if let Some(apps) = cached {
        return Ok(apps);
    }

    // Cache miss - fetch new content
    let apps = fetch_applications()?;

    // Update thread-local cache
    APPS_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(apps.clone());
    });

    Ok(apps)
}

/// Refresh the applications cache (call if you need fresh app list)
#[allow(dead_code)]
pub fn refresh_applications_cache() -> Result<(), DevicesError> {
    let apps = fetch_applications()?;
    APPS_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(apps);
    });
    Ok(())
}

/// Invalidate the thread-local display cache
#[allow(dead_code)]
pub fn invalidate_display_cache() {
    DISPLAY_CACHE.with(|cache| {
        *cache.borrow_mut() = None;
    });
}

/// Invalidate the thread-local applications cache
#[allow(dead_code)]
pub fn invalidate_applications_cache() {
    APPS_CACHE.with(|cache| {
        *cache.borrow_mut() = None;
    });
}

pub struct Devices(VecIntoIter<Device>);

impl Devices {
    pub fn new() -> Result<Self, DevicesError> {
        let displays = get_displays_cached()?;
        let mut res = Vec::new();

        for display in displays.iter() {
            res.push(Device::new(display.clone()));
        }

        Ok(Devices(res.into_iter()))
    }

    /// Get available running applications (uses cache, ~1µs on cache hit)
    pub fn available_applications() -> Result<Vec<Retained<SCRunningApplication>>, DevicesError> {
        get_applications_cached()
    }
}

unsafe impl Send for Devices {}
unsafe impl Sync for Devices {}

impl Iterator for Devices {
    type Item = Device;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

pub fn default_input_device() -> Option<Device> {
    let devices = Devices::new().ok()?;
    devices.into_iter().next()
}

pub fn default_output_device() -> Option<Device> {
    None
}

pub type SupportedInputConfigs = VecIntoIter<SupportedStreamConfigRange>;
pub type SupportedOutputConfigs = VecIntoIter<SupportedStreamConfigRange>;
