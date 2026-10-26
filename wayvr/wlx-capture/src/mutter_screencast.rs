//! LVRR: GNOME's compositor records a monitor on request over D-Bus
//! (org.gnome.Mutter.ScreenCast), the way the GNOME portal gets its streams, so no
//! picker dialog is involved.

use std::{
    any::Any,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use dbus::{
    Message, Path,
    arg::{PropMap, RefArg, Variant},
    blocking::Connection,
    message::MatchRule,
};

use crate::{DrmFormat, WlxCapture, frame::WlxFrame, pipewire::capture::PipewireCapture};

const BUS: &str = "org.gnome.Mutter.ScreenCast";
const OBJECT: &str = "/org/gnome/Mutter/ScreenCast";
const SESSION: &str = "org.gnome.Mutter.ScreenCast.Session";
const STREAM: &str = "org.gnome.Mutter.ScreenCast.Stream";
const TIMEOUT: Duration = Duration::from_secs(5);

// cursor-mode of RecordMonitor
const CURSOR_HIDDEN: u32 = 0;
const CURSOR_EMBEDDED: u32 = 1;

pub struct MutterScreencastCapture<R>
where
    R: Any + Send,
{
    inner: PipewireCapture<R>,
    /// the session lives as long as this connection
    conn: Connection,
    session: Path<'static>,
}

impl<R> MutterScreencastCapture<R>
where
    R: Any + Send,
{
    /// Asks the compositor to record the monitor on the connector, which is the
    /// wl_output name. None when no Mutter owns the name, it refused, or did not answer
    /// in time.
    pub fn new(name: Arc<str>, connector: &str, embed_cursor: bool) -> Option<Self> {
        let conn = Connection::new_session()
            .inspect_err(|e| log::debug!("session bus: {e}"))
            .ok()?;

        // only the running compositor, never an activation attempt
        let bus = conn.with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", TIMEOUT);
        let (owned,): (bool,) = bus
            .method_call("org.freedesktop.DBus", "NameHasOwner", (BUS,))
            .ok()?;
        if !owned {
            return None;
        }

        let manager = conn.with_proxy(BUS, OBJECT, TIMEOUT);
        let (session,): (Path<'static>,) = manager
            .method_call(BUS, "CreateSession", (PropMap::new(),))
            .inspect_err(|e| log::warn!("{name}: Mutter screencast: CreateSession: {e}"))
            .ok()?;

        let mut props = PropMap::new();
        let cursor = if embed_cursor {
            CURSOR_EMBEDDED
        } else {
            CURSOR_HIDDEN
        };
        props.insert(
            "cursor-mode".into(),
            Variant(Box::new(cursor) as Box<dyn RefArg>),
        );
        let session_proxy = conn.with_proxy(BUS, session.clone(), TIMEOUT);
        let (stream,): (Path<'static>,) = session_proxy
            .method_call(SESSION, "RecordMonitor", (connector, props))
            .inspect_err(|e| log::warn!("{name}: Mutter screencast: RecordMonitor: {e}"))
            .ok()?;

        // the node id comes as a signal once the session started
        let node: Arc<Mutex<Option<u32>>> = Arc::default();
        let rule = MatchRule::new_signal(STREAM, "PipeWireStreamAdded").with_path(stream);
        let token = conn
            .add_match(rule, {
                let node = node.clone();
                move |(id,): (u32,), _: &Connection, _: &Message| {
                    if let Ok(mut node) = node.lock() {
                        *node = Some(id);
                    }
                    true
                }
            })
            .ok()?;

        let started: Result<(), _> = session_proxy.method_call(SESSION, "Start", ());
        if let Err(e) = started {
            log::warn!("{name}: Mutter screencast: Start: {e}");
            let _ = conn.remove_match(token);
            return None;
        }

        let deadline = Instant::now() + TIMEOUT;
        let node = loop {
            if let Some(id) = node.lock().ok().and_then(|n| *n) {
                break id;
            }
            if Instant::now() >= deadline {
                log::warn!(
                    "{name}: Mutter screencast: no stream in {}s",
                    TIMEOUT.as_secs()
                );
                let _: Result<(), _> = session_proxy.method_call(SESSION, "Stop", ());
                let _ = conn.remove_match(token);
                return None;
            }
            let _ = conn.process(Duration::from_millis(100));
        };
        let _ = conn.remove_match(token);

        log::info!("{name}: Mutter screencast: PipeWire node {node}");
        Some(Self {
            inner: PipewireCapture::new(name, node),
            conn,
            session,
        })
    }
}

impl<R> Drop for MutterScreencastCapture<R>
where
    R: Any + Send,
{
    fn drop(&mut self) {
        let session = self.conn.with_proxy(BUS, self.session.clone(), TIMEOUT);
        let _: Result<(), _> = session.method_call(SESSION, "Stop", ());
    }
}

impl<U, R> WlxCapture<U, R> for MutterScreencastCapture<R>
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
