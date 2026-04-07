//! Stream implementation for WasapiLoopback.
//!
//! Wraps the WASAPI stream in a distinct type to avoid conflicting `From` impls
//! in the `impl_platform_host!` macro, which requires each host's Stream type to be unique.

use crate::host::wasapi::stream as wasapi_stream;
use crate::traits::StreamTrait;
use crate::{Data, InputCallbackInfo, PauseStreamError, PlayStreamError, StreamError};

pub use wasapi_stream::StreamInner;

/// A loopback capture stream wrapping WASAPI's stream infrastructure.
///
/// This is a newtype wrapper around `wasapi::stream::Stream` to give it a distinct
/// type identity, required by cpal's `impl_platform_host!` macro.
pub struct Stream(wasapi_stream::Stream);

unsafe impl Send for Stream {}
unsafe impl Sync for Stream {}

crate::assert_stream_send!(Stream);
crate::assert_stream_sync!(Stream);

impl Stream {
    pub(crate) fn new_input<D, E>(
        stream_inner: StreamInner,
        data_callback: D,
        error_callback: E,
    ) -> Stream
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        Stream(wasapi_stream::Stream::new_input(
            stream_inner,
            data_callback,
            error_callback,
        ))
    }
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), PlayStreamError> {
        self.0.play()
    }

    fn pause(&self) -> Result<(), PauseStreamError> {
        self.0.pause()
    }
}
