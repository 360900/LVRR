//! LVRR: KWin streams an output over PipeWire on request (zkde_screencast_unstable_v1),
//! the way the KDE portal gets its streams, so no picker dialog is involved.

use std::{
    any::Any,
    sync::Arc,
    time::{Duration, Instant},
};

use wayland_client::Proxy;
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

/// zkde_screencast_unstable_v1 version with stream_virtual_output
const VIRTUAL_OUTPUT_SINCE: u32 = 2;

pub struct KdeScreencastCapture<R>
where
    R: Any + Send,
{
    inner: PipewireCapture<R>,
    /// after `inner`, so the capture stops before the stream closes
    stream: KdeStream,
}

impl<R> KdeScreencastCapture<R>
where
    R: Any + Send,
{
    /// Asks the compositor for a PipeWire stream of the output. None when it offers no
    /// zkde_screencast_unstable_v1, refused, or did not answer in time.
    pub fn new(client: Box<WlxClient>, output_id: u32, embed_cursor: bool) -> Option<Self> {
        KdeStream::output(client, output_id, embed_cursor).map(Self::from_stream)
    }

    pub fn from_stream(stream: KdeStream) -> Self {
        Self {
            inner: PipewireCapture::new(stream.name.clone(), stream.node),
            stream,
        }
    }
}

/// A stream the compositor made. It has no frame type yet, so another thread can ask
/// for it.
pub struct KdeStream {
    name: Arc<str>,
    node: u32,
    stream: ZkdeScreencastStreamUnstableV1,
    /// keeps the connection, and with it the stream, alive
    client: Box<WlxClient>,
}

impl KdeStream {
    fn output(client: Box<WlxClient>, output_id: u32, embed_cursor: bool) -> Option<Self> {
        let manager = client.maybe_kde_screencast_mgr.clone()?;
        let (name, wl_output) = {
            let output = client.outputs.get(output_id)?;
            (output.name.clone(), output.wl_output.clone())
        };
        let result: KdeStreamResult = Arc::default();
        let stream = manager.stream_output(
            &wl_output,
            pointer_mode(embed_cursor),
            &client.queue_handle,
            result.clone(),
        );
        Self::wait(client, name, stream, &result)
    }

    /// Asks the compositor to add an output of this logical size and stream it. KWin
    /// names it `Virtual-<name>`. The output goes away with the stream. None when the compositor can't
    /// add outputs, refused, or did not answer in time.
    pub fn virtual_output(
        client: Box<WlxClient>,
        name: Arc<str>,
        width: u32,
        height: u32,
        embed_cursor: bool,
    ) -> Option<Self> {
        let manager = client
            .maybe_kde_screencast_mgr
            .clone()
            .filter(|m| m.version() >= VIRTUAL_OUTPUT_SINCE)?;
        let result: KdeStreamResult = Arc::default();
        let stream = manager.stream_virtual_output(
            name.to_string(),
            width as i32,
            height as i32,
            1.0,
            pointer_mode(embed_cursor),
            &client.queue_handle,
            result.clone(),
        );
        Self::wait(client, name, stream, &result)
    }

    fn wait(
        mut client: Box<WlxClient>,
        name: Arc<str>,
        stream: ZkdeScreencastStreamUnstableV1,
        result: &KdeStreamResult,
    ) -> Option<Self> {
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
                    name,
                    node,
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

/// Whether the compositor can add outputs to stream (`KdeStream::virtual_output`)
pub fn supports_virtual(client: &WlxClient) -> bool {
    client
        .maybe_kde_screencast_mgr
        .as_ref()
        .is_some_and(|m| m.version() >= VIRTUAL_OUTPUT_SINCE)
}

const fn pointer_mode(embed_cursor: bool) -> u32 {
    if embed_cursor {
        POINTER_EMBEDDED
    } else {
        POINTER_HIDDEN
    }
}

impl Drop for KdeStream {
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
