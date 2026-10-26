//! LVRR: KWin streams an output over PipeWire on request (zkde_screencast_unstable_v1),
//! the way the KDE portal gets its streams, so no picker dialog is involved.

use std::{
    any::Any,
    sync::Arc,
    time::{Duration, Instant},
};

use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::ZkdeScreencastStreamUnstableV1;

use crate::{
    DrmFormat, WlxCapture,
    frame::WlxFrame,
    pipewire::capture::PipewireCapture,
    wayland::{KdeStreamResult, WlxClient},
};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

// pointer modes of zkde_screencast_unstable_v1
const POINTER_HIDDEN: u32 = 1;
const POINTER_EMBEDDED: u32 = 2;

pub struct KdeScreencastCapture<R>
where
    R: Any + Send,
{
    inner: PipewireCapture<R>,
    stream: ZkdeScreencastStreamUnstableV1,
    /// keeps the connection, and with it the stream, alive
    client: Box<WlxClient>,
}

impl<R> KdeScreencastCapture<R>
where
    R: Any + Send,
{
    /// Asks the compositor for a PipeWire stream of the output. None when it offers no
    /// zkde_screencast_unstable_v1, refused, or did not answer in time.
    pub fn new(mut client: Box<WlxClient>, output_id: u32, embed_cursor: bool) -> Option<Self> {
        let manager = client.maybe_kde_screencast_mgr.clone()?;
        let (name, wl_output) = {
            let output = client.outputs.get(output_id)?;
            (output.name.clone(), output.wl_output.clone())
        };

        let pointer = if embed_cursor {
            POINTER_EMBEDDED
        } else {
            POINTER_HIDDEN
        };
        let result: KdeStreamResult = Arc::default();
        let stream =
            manager.stream_output(&wl_output, pointer, &client.queue_handle, result.clone());
        let _ = client.connection.flush();

        let deadline = Instant::now() + ANSWER_TIMEOUT;
        let answer = loop {
            if let Some(answer) = result.lock().ok().and_then(|r| r.clone()) {
                break answer;
            }
            if Instant::now() >= deadline {
                log::warn!(
                    "{name}: KDE screencast: no answer in {}s",
                    ANSWER_TIMEOUT.as_secs()
                );
                stream.close();
                let _ = client.connection.flush();
                return None;
            }
            client.wait_readable(deadline);
            client.dispatch_pending();
        };

        match answer {
            Ok(node) => {
                log::info!("{name}: KDE screencast: PipeWire node {node}");
                Some(Self {
                    inner: PipewireCapture::new(name, node),
                    stream,
                    client,
                })
            }
            Err(error) => {
                log::warn!("{name}: KDE screencast refused: {error}");
                stream.close();
                let _ = client.connection.flush();
                None
            }
        }
    }
}

impl<R> Drop for KdeScreencastCapture<R>
where
    R: Any + Send,
{
    fn drop(&mut self) {
        self.stream.close();
        let _ = self.client.connection.flush();
    }
}

impl<U, R> WlxCapture<U, R> for KdeScreencastCapture<R>
where
    U: Any + Send,
    R: Any + Send,
{
    fn init(
        &mut self,
        dmabuf_formats: &[DrmFormat],
        user_data: U,
        callback: fn(&U, WlxFrame) -> Option<R>,
    ) {
        self.inner.init(dmabuf_formats, user_data, callback);
    }
    fn is_ready(&self) -> bool {
        <PipewireCapture<R> as WlxCapture<U, R>>::is_ready(&self.inner)
    }
    fn supports_dmbuf(&self) -> bool {
        <PipewireCapture<R> as WlxCapture<U, R>>::supports_dmbuf(&self.inner)
    }
    fn receive(&mut self) -> Option<R> {
        <PipewireCapture<R> as WlxCapture<U, R>>::receive(&mut self.inner)
    }
    fn pause(&mut self) {
        <PipewireCapture<R> as WlxCapture<U, R>>::pause(&mut self.inner);
    }
    fn resume(&mut self) {
        <PipewireCapture<R> as WlxCapture<U, R>>::resume(&mut self.inner);
    }
    fn request_new_frame(&mut self) {
        <PipewireCapture<R> as WlxCapture<U, R>>::request_new_frame(&mut self.inner);
    }
}
