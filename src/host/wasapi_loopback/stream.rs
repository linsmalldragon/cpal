//! Stream implementation for WasapiLoopback.
//!
//! Wraps the WASAPI stream in a distinct type to avoid conflicting `From` impls
//! in the `impl_platform_host!` macro, which requires each host's Stream type to be unique.
//!
//! Supports two modes:
//! - `Single`: Classic loopback or single-PID EXCLUDE (delegates to `wasapi::stream::Stream`)
//! - `MultiExclude`: Audio subtraction for 2+ excluded PIDs (custom capture thread)

use crate::host::wasapi::stream as wasapi_stream;
use crate::host::wasapi_loopback::CaptureComponents;
use crate::traits::StreamTrait;
use crate::{Data, InputCallbackInfo, PauseStreamError, PlayStreamError, SampleFormat, StreamError};

use std::collections::VecDeque;
use std::ptr;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::{self, JoinHandle};
use windows::Win32::Foundation;
use windows::Win32::Media::Audio;
use windows::Win32::System::{Performance, Threading};

pub use wasapi_stream::StreamInner;

/// A loopback capture stream.
///
/// This enum supports two capture modes:
/// - `Single`: Wraps `wasapi::stream::Stream` for classic loopback or single-PID EXCLUDE.
/// - `MultiExclude`: Custom capture thread that performs audio subtraction for multi-PID exclusion.
pub enum Stream {
    /// Classic loopback or single-PID EXCLUDE (delegates to wasapi stream)
    Single(wasapi_stream::Stream),
    /// Audio subtraction: Total - Include_0 - Include_1 - ... for 2+ excluded PIDs
    MultiExclude(MultiExcludeStream),
}

unsafe impl Send for Stream {}
unsafe impl Sync for Stream {}

crate::assert_stream_send!(Stream);
crate::assert_stream_sync!(Stream);

impl Stream {
    /// Create a stream from a single StreamInner (classic loopback or single-PID EXCLUDE)
    pub(crate) fn new_input<D, E>(
        stream_inner: StreamInner,
        data_callback: D,
        error_callback: E,
    ) -> Stream
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        Stream::Single(wasapi_stream::Stream::new_input(
            stream_inner,
            data_callback,
            error_callback,
        ))
    }

    /// Create a multi-exclude stream for audio subtraction
    pub(crate) fn new_multi_exclude<D, E>(
        total: CaptureComponents,
        includes: Vec<CaptureComponents>,
        sample_format: SampleFormat,
        bytes_per_frame: u16,
        data_callback: D,
        error_callback: E,
    ) -> Stream
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        Stream::MultiExclude(MultiExcludeStream::new(
            total,
            includes,
            sample_format,
            bytes_per_frame,
            data_callback,
            error_callback,
        ))
    }
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), PlayStreamError> {
        match self {
            Stream::Single(s) => s.play(),
            Stream::MultiExclude(s) => s.play(),
        }
    }

    fn pause(&self) -> Result<(), PauseStreamError> {
        match self {
            Stream::Single(s) => s.pause(),
            Stream::MultiExclude(s) => s.pause(),
        }
    }
}

// ============================================================================
// MultiExcludeStream implementation
// ============================================================================

enum Command {
    PlayStream,
    PauseStream,
    Terminate,
}

/// A capture source wrapping one IAudioClient + IAudioCaptureClient pair.
struct CaptureSource {
    audio_client: Audio::IAudioClient,
    capture_client: Audio::IAudioCaptureClient,
    event: Foundation::HANDLE,
    #[allow(dead_code)]
    max_frames: u32,
    /// Ring buffer holding f32 samples drained from this source
    buffer: VecDeque<f32>,
    /// Set to true when this source encounters an error (e.g., process exited)
    dead: bool,
}

impl CaptureSource {
    fn from_components(components: CaptureComponents) -> Self {
        CaptureSource {
            audio_client: components.audio_client,
            capture_client: components.capture_client,
            event: components.event,
            max_frames: components.max_frames_in_buffer,
            buffer: VecDeque::with_capacity(components.max_frames_in_buffer as usize * 2),
            dead: false,
        }
    }
}

impl Drop for CaptureSource {
    fn drop(&mut self) {
        unsafe {
            let _ = Foundation::CloseHandle(self.event);
        }
    }
}

/// Audio subtraction stream for multi-PID exclusion.
///
/// Manages 1 Total capture (classic loopback) + N Include captures (one per excluded PID).
/// On each capture cycle: Total - Include_0 - Include_1 - ... = output.
pub struct MultiExcludeStream {
    thread: Option<JoinHandle<()>>,
    commands: Sender<Command>,
    pending_scheduled_event: Foundation::HANDLE,
}

unsafe impl Send for MultiExcludeStream {}
unsafe impl Sync for MultiExcludeStream {}

/// Thread context for the multi-exclude capture thread.
/// Groups all data that needs to be moved into the thread.
struct MultiExcludeContext {
    cmd_event: Foundation::HANDLE,
    commands: Receiver<Command>,
    total: CaptureComponents,
    includes: Vec<CaptureComponents>,
    sample_format: SampleFormat,
    bytes_per_frame: u16,
}

// SAFETY: All contained Windows objects (HANDLE, IAudioClient, IAudioCaptureClient) are safe
// to access from a single thread. This context is moved into the capture thread and used
// exclusively from there. Same reasoning as wasapi::stream::RunContext.
unsafe impl Send for MultiExcludeContext {}

impl MultiExcludeStream {
    fn new<D, E>(
        total: CaptureComponents,
        includes: Vec<CaptureComponents>,
        sample_format: SampleFormat,
        bytes_per_frame: u16,
        mut data_callback: D,
        mut error_callback: E,
    ) -> Self
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        let pending_scheduled_event = unsafe {
            Threading::CreateEventA(None, false, false, windows::core::PCSTR(ptr::null()))
        }
        .expect("cpal: could not create multi-exclude command event");

        let (tx, rx) = channel();

        let ctx = MultiExcludeContext {
            cmd_event: pending_scheduled_event,
            commands: rx,
            total,
            includes,
            sample_format,
            bytes_per_frame,
        };

        let thread = thread::Builder::new()
            .name("cpal_wasapi_loopback_multi".to_owned())
            .spawn(move || {
                run_multi_exclude(
                    ctx,
                    &mut data_callback,
                    &mut error_callback,
                );
            })
            .unwrap();

        MultiExcludeStream {
            thread: Some(thread),
            commands: tx,
            pending_scheduled_event,
        }
    }

    fn play(&self) -> Result<(), PlayStreamError> {
        self.commands
            .send(Command::PlayStream)
            .map_err(|_| PlayStreamError::DeviceNotAvailable)?;
        unsafe {
            let _ = Threading::SetEvent(self.pending_scheduled_event);
        }
        Ok(())
    }

    fn pause(&self) -> Result<(), PauseStreamError> {
        self.commands
            .send(Command::PauseStream)
            .map_err(|_| PauseStreamError::DeviceNotAvailable)?;
        unsafe {
            let _ = Threading::SetEvent(self.pending_scheduled_event);
        }
        Ok(())
    }
}

impl Drop for MultiExcludeStream {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Terminate);
        unsafe {
            let _ = Threading::SetEvent(self.pending_scheduled_event);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        unsafe {
            let _ = Foundation::CloseHandle(self.pending_scheduled_event);
        }
    }
}

// ============================================================================
// Multi-exclude capture thread
// ============================================================================

fn run_multi_exclude<D, E>(
    ctx: MultiExcludeContext,
    data_callback: &mut D,
    error_callback: &mut E,
) where
    D: FnMut(&Data, &InputCallbackInfo),
    E: FnMut(StreamError),
{
    use crate::host::wasapi::com;
    com::com_initialized();

    let cmd_event = ctx.cmd_event;
    let commands = ctx.commands;
    let sample_format = ctx.sample_format;
    let bytes_per_frame = ctx.bytes_per_frame;

    // Build sources: index 0 = Total, indices 1..N = Include captures
    let mut sources: Vec<CaptureSource> = Vec::with_capacity(1 + ctx.includes.len());
    sources.push(CaptureSource::from_components(ctx.total));
    for inc in ctx.includes {
        sources.push(CaptureSource::from_components(inc));
    }

    let mut playing = false;

    loop {
        // Build handles array: [cmd_event, total_event, include_0_event, ...]
        // Only include events from live sources
        let mut handles: Vec<Foundation::HANDLE> = vec![cmd_event];
        for src in sources.iter() {
            if !src.dead {
                handles.push(src.event);
            }
        }

        // Wait for any event (10ms timeout to allow periodic buffer processing)
        let result = unsafe {
            Threading::WaitForMultipleObjectsEx(&handles, false, 10, false)
        };
        // Save GetLastError immediately — before any other calls can overwrite it
        let last_error = if result == Foundation::WAIT_FAILED {
            Some(unsafe { Foundation::GetLastError() })
        } else {
            None
        };

        // Process commands first
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                Command::PlayStream => {
                    if !playing {
                        for src in &sources {
                            if !src.dead {
                                unsafe {
                                    let _ = src.audio_client.Start();
                                }
                            }
                        }
                        playing = true;
                    }
                }
                Command::PauseStream => {
                    if playing {
                        for src in &sources {
                            if !src.dead {
                                unsafe {
                                    let _ = src.audio_client.Stop();
                                }
                            }
                        }
                        playing = false;
                    }
                }
                Command::Terminate => {
                    for src in &sources {
                        if !src.dead {
                            unsafe {
                                let _ = src.audio_client.Stop();
                            }
                        }
                    }
                    return;
                }
            }
        }

        if let Some(err) = last_error {
            let description = format!("WaitForMultipleObjectsEx failed in multi-exclude stream: {:?}", err);
            error_callback(StreamError::BackendSpecific {
                err: crate::BackendSpecificError { description },
            });
            return;
        }

        if !playing {
            continue;
        }

        // Drain all sources (regardless of which event fired)
        for src in &mut sources {
            if src.dead {
                continue;
            }
            drain_source(src, sample_format, bytes_per_frame, error_callback);
        }

        // Perform subtraction: output = Total - Include_0 - Include_1 - ...
        // Only proceed when Total has data
        if sources[0].dead {
            // Total source died (e.g., audio endpoint disconnected).
            // This is a fatal error — we can't produce any output without the Total capture.
            for src in &sources[1..] {
                if !src.dead {
                    unsafe {
                        let _ = src.audio_client.Stop();
                    }
                }
            }
            error_callback(StreamError::DeviceNotAvailable);
            return;
        }
        if sources[0].buffer.is_empty() {
            continue;
        }

        // Use Total's buffer length as the output length.
        // Include sources that have less data use 0.0 for missing samples
        // (they may be silent or lagging). This prevents Total's buffer from
        // growing unboundedly when Include sources are silent.
        let output_len = sources[0].buffer.len();

        // Build the subtraction result
        let mut result_buf: Vec<f32> = Vec::with_capacity(output_len);
        for _ in 0..output_len {
            let mut sample = sources[0].buffer.pop_front().unwrap();
            for src in &mut sources[1..] {
                // Drain residual buffer even from dead sources before skipping them
                if let Some(s) = src.buffer.pop_front() {
                    sample -= s;
                }
            }
            result_buf.push(sample.clamp(-1.0, 1.0));
        }

        // Cap Include buffers to prevent unbounded growth if they accumulate
        // faster than Total (shouldn't happen normally, but defensive)
        const MAX_RING_BUFFER_SAMPLES: usize = 48000 * 2; // ~1 second of stereo 48kHz
        for src in &mut sources[1..] {
            if src.buffer.len() > MAX_RING_BUFFER_SAMPLES {
                let excess = src.buffer.len() - MAX_RING_BUFFER_SAMPLES;
                src.buffer.drain(..excess);
            }
        }

        // Deliver to callback
        // SAFETY: result_buf is alive for the duration of the callback.
        // Data::from_parts wraps the pointer without taking ownership.
        let data = unsafe {
            Data::from_parts(
                result_buf.as_ptr() as *mut (),
                result_buf.len(),
                SampleFormat::F32,
            )
        };

        let callback_instant = current_qpc_stream_instant();
        let timestamp = crate::InputStreamTimestamp {
            // For multi-source subtraction, capture time ≈ callback time
            // (precise per-source capture times can't be unified into one value)
            capture: callback_instant,
            callback: callback_instant,
        };
        let info = InputCallbackInfo::new(timestamp);
        data_callback(&data, &info);
    }
}

/// Get the current time as a `StreamInstant` using QueryPerformanceCounter.
fn current_qpc_stream_instant() -> crate::StreamInstant {
    use std::sync::OnceLock;
    static QPC_FREQ: OnceLock<i64> = OnceLock::new();

    let freq = *QPC_FREQ.get_or_init(|| {
        let mut f: i64 = 0;
        unsafe {
            let _ = Performance::QueryPerformanceFrequency(&mut f);
        }
        f
    });

    let mut qpc: i64 = 0;
    unsafe {
        let _ = Performance::QueryPerformanceCounter(&mut qpc);
    }
    if freq == 0 {
        return crate::StreamInstant::new(0, 0);
    }
    let nanos = (qpc as i128 * 1_000_000_000) / freq as i128;
    crate::StreamInstant::from_nanos_i128(nanos).unwrap_or(crate::StreamInstant::new(0, 0))
}

/// Buffer flags constant: the data should be treated as silence.
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;

/// Drain all available packets from a capture source into its ring buffer as f32 samples.
fn drain_source(
    src: &mut CaptureSource,
    sample_format: SampleFormat,
    bytes_per_frame: u16,
    _error_callback: &mut dyn FnMut(StreamError),
) {
    unsafe {
        loop {
            match src.capture_client.GetNextPacketSize() {
                Ok(0) => return,
                Ok(_) => {}
                Err(_) => {
                    // Source errored (e.g., process exited for INCLUDE captures)
                    src.dead = true;
                    let _ = src.audio_client.Stop();
                    // Don't report as error for include sources - it's expected when processes exit
                    return;
                }
            };

            let mut buffer: *mut u8 = ptr::null_mut();
            let mut frames_available: u32 = 0;
            let mut flags: u32 = 0;
            let result = src.capture_client.GetBuffer(
                &mut buffer,
                &mut frames_available,
                &mut flags,
                None,
                None,
            );

            match result {
                Err(_) => {
                    src.dead = true;
                    let _ = src.audio_client.Stop();
                    return;
                }
                Ok(_) => (),
            }

            if buffer.is_null() || frames_available == 0 {
                let _ = src.capture_client.ReleaseBuffer(frames_available);
                continue;
            }

            let total_samples = frames_available as usize * bytes_per_frame as usize / sample_format.sample_size();

            // If the SILENT flag is set, the buffer may contain stale data — push zeros instead
            if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 {
                for _ in 0..total_samples {
                    src.buffer.push_back(0.0);
                }
            } else {
                // Convert to f32 and push into ring buffer
                match sample_format {
                    SampleFormat::F32 => {
                        let slice = std::slice::from_raw_parts(buffer as *const f32, total_samples);
                        src.buffer.extend(slice.iter().copied());
                    }
                    SampleFormat::I16 => {
                        let slice = std::slice::from_raw_parts(buffer as *const i16, total_samples);
                        for &s in slice {
                            src.buffer.push_back(s as f32 / 32768.0);
                        }
                    }
                    SampleFormat::I32 => {
                        let slice = std::slice::from_raw_parts(buffer as *const i32, total_samples);
                        for &s in slice {
                            src.buffer.push_back(s as f32 / 2147483648.0);
                        }
                    }
                    SampleFormat::U8 => {
                        let slice = std::slice::from_raw_parts(buffer, total_samples);
                        for &s in slice {
                            src.buffer.push_back((s as f32 - 128.0) / 128.0);
                        }
                    }
                    SampleFormat::I24 => {
                        let bytes = std::slice::from_raw_parts(buffer, total_samples * 3);
                        for i in 0..total_samples {
                            let b0 = bytes[i * 3] as i32;
                            let b1 = bytes[i * 3 + 1] as i32;
                            let b2 = bytes[i * 3 + 2] as i32;
                            let raw = b0 | (b1 << 8) | (b2 << 16);
                            let val = (raw << 8) >> 8; // sign extend 24→32
                            src.buffer.push_back(val as f32 / 8388608.0);
                        }
                    }
                    SampleFormat::U24 => {
                        let bytes = std::slice::from_raw_parts(buffer, total_samples * 3);
                        for i in 0..total_samples {
                            let b0 = bytes[i * 3] as u32;
                            let b1 = bytes[i * 3 + 1] as u32;
                            let b2 = bytes[i * 3 + 2] as u32;
                            let raw = b0 | (b1 << 8) | (b2 << 16);
                            src.buffer.push_back(raw as f32 / 8388608.0 - 1.0);
                        }
                    }
                    SampleFormat::I8 => {
                        let slice = std::slice::from_raw_parts(buffer as *const i8, total_samples);
                        for &s in slice {
                            src.buffer.push_back(s as f32 / 128.0);
                        }
                    }
                    SampleFormat::U16 => {
                        let slice = std::slice::from_raw_parts(buffer as *const u16, total_samples);
                        for &s in slice {
                            src.buffer.push_back(s as f32 / 32768.0 - 1.0);
                        }
                    }
                    SampleFormat::U32 => {
                        let slice = std::slice::from_raw_parts(buffer as *const u32, total_samples);
                        for &s in slice {
                            src.buffer.push_back((s as f64 / 2147483648.0 - 1.0) as f32);
                        }
                    }
                    SampleFormat::I64 => {
                        let slice = std::slice::from_raw_parts(buffer as *const i64, total_samples);
                        for &s in slice {
                            src.buffer.push_back((s as f64 / 9223372036854775808.0) as f32);
                        }
                    }
                    SampleFormat::F64 => {
                        let slice = std::slice::from_raw_parts(buffer as *const f64, total_samples);
                        for &s in slice {
                            src.buffer.push_back(s as f32);
                        }
                    }
                    _ => {
                        // Unknown format — push silence to keep buffers synchronized
                        for _ in 0..total_samples {
                            src.buffer.push_back(0.0);
                        }
                    }
                }
            }

            let _ = src.capture_client.ReleaseBuffer(frames_available);
        }
    }
}
