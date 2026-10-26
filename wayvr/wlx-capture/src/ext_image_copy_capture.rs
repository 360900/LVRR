//! LVRR: ext-image-copy-capture-v1, the standard successor of wlr-screencopy (wlroots 0.19,
//! Hyprland, niri, COSMIC). One session per output; for every frame the client attaches a
//! buffer of its own and the compositor copies the screen into it once it changed.

use std::{
    any::Any,
    ffi::CString,
    os::fd::{BorrowedFd, RawFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use drm_fourcc::{DrmFormat, DrmFourcc, DrmModifier};
use libc::{O_CREAT, O_RDWR, S_IRUSR, S_IWUSR};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
    protocol::{wl_buffer::WlBuffer, wl_shm::Format, wl_shm_pool::WlShmPool},
};
use wayland_protocols::{
    ext::{
        image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        image_copy_capture::v1::client::{
            ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
            ext_image_copy_capture_manager_v1::Options,
            ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
        },
    },
    wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1,
};

use crate::{
    WlxCapture,
    frame::{self, DmaExporter, FrameFormat, FramePlane, MemFdFrame, WlxFrame},
    wayland::{WlxClient, wl_transform_to_frame_transform},
};

const CONSTRAINTS_TIMEOUT: Duration = Duration::from_secs(5);

/// formats this crate can hand to the renderer, best first
const FORMATS: [DrmFourcc; 4] = [
    DrmFourcc::Argb8888,
    DrmFourcc::Xrgb8888,
    DrmFourcc::Abgr8888,
    DrmFourcc::Xbgr8888,
];

/// The buffer constraints of a session, from its events
#[derive(Default)]
struct SessionInfo {
    size: (u32, u32),
    shm_formats: Vec<Format>,
    /// format code and the modifiers allowed with it
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
    /// the last batch of constraints ended
    done: bool,
    stopped: bool,
    /// the compositor refused a dma-buf, stay with shared memory
    dmabuf_failed: bool,
}

impl SessionInfo {
    /// A constraint event after `done` starts a new batch
    fn updating(&mut self) {
        if self.done {
            self.done = false;
            self.shm_formats.clear();
            self.dmabuf_formats.clear();
        }
    }
}

type SessionState = Arc<Mutex<SessionInfo>>;

struct Session {
    source: ExtImageCaptureSourceV1,
    session: ExtImageCopyCaptureSessionV1,
    info: SessionState,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.session.destroy();
        self.source.destroy();
    }
}

enum FrameEvent {
    Transform(frame::Transform),
    Ready,
    Failed(Option<FailureReason>),
}

enum BufData {
    Shm {
        wl_buffer: WlBuffer,
        wl_pool: WlShmPool,
        fd: RawFd,
    },
    Dma {
        wl_buffer: WlBuffer,
    },
}

impl Drop for BufData {
    fn drop(&mut self) {
        match self {
            Self::Shm {
                wl_buffer,
                wl_pool,
                fd,
            } => {
                wl_buffer.destroy();
                wl_pool.destroy();
                unsafe {
                    libc::close(*fd);
                }
            }
            Self::Dma { wl_buffer } => {
                wl_buffer.destroy();
            }
        }
    }
}

struct CaptureData<U, R> {
    sender: mpsc::SyncSender<R>,
    receiver: mpsc::Receiver<R>,
    user_data: Option<Box<U>>,
    receive_callback: fn(&U, WlxFrame) -> Option<R>,
}

/// What the worker thread hands back
type Worker<U> = (Box<WlxClient>, Box<U>, Session);

pub struct ExtImageCopyCapture<U, R> {
    output_id: u32,
    wl: Option<Box<WlxClient>>,
    session: Option<Session>,
    handle: Option<JoinHandle<Worker<U>>>,
    data: Option<CaptureData<U, R>>,
}

impl<U, R> ExtImageCopyCapture<U, R>
where
    U: Any + Send + DmaExporter,
    R: Any + Send,
{
    /// Opens a capture session for the output. None when the compositor offers no
    /// ext-image-copy-capture, stops the session at once or sends no constraints in time.
    pub fn new(mut client: Box<WlxClient>, output_id: u32) -> Option<Self> {
        let manager = client.maybe_ext_image_copy_capture_mgr.clone()?;
        let source_manager = client.maybe_ext_output_source_mgr.clone()?;
        let (name, wl_output) = {
            let output = client.outputs.get(output_id)?;
            (output.name.clone(), output.wl_output.clone())
        };

        let info: SessionState = Arc::default();
        let source = source_manager.create_source(&wl_output, &client.queue_handle, ());
        let session = manager.create_session(
            &source,
            Options::PaintCursors,
            &client.queue_handle,
            info.clone(),
        );
        let session = Session {
            source,
            session,
            info,
        };
        let _ = client.connection.flush();

        // the constraints come right after creation, ending with done
        let deadline = Instant::now() + CONSTRAINTS_TIMEOUT;
        loop {
            {
                let info = session.info.lock().ok()?;
                if info.stopped {
                    log::warn!("{name}: image copy capture: the compositor stopped the session");
                    return None;
                }
                if info.done {
                    log::info!(
                        "{name}: image copy capture: {}x{}, {} shm and {} dma-buf formats",
                        info.size.0,
                        info.size.1,
                        info.shm_formats.len(),
                        info.dmabuf_formats.len()
                    );
                    break;
                }
            }
            if Instant::now() >= deadline {
                log::warn!(
                    "{name}: image copy capture: no buffer constraints in {}s",
                    CONSTRAINTS_TIMEOUT.as_secs()
                );
                return None;
            }
            client.wait_readable(deadline);
            client.dispatch_pending();
        }

        Some(Self {
            output_id,
            wl: Some(client),
            session: Some(session),
            handle: None,
            data: None,
        })
    }
}

impl<U, R> WlxCapture<U, R> for ExtImageCopyCapture<U, R>
where
    U: Any + Send + DmaExporter,
    R: Any + Send,
{
    fn init(
        &mut self,
        _: &[DrmFormat],
        user_data: U,
        receive_callback: fn(&U, WlxFrame) -> Option<R>,
    ) {
        debug_assert!(self.wl.is_some());

        let (sender, receiver) = mpsc::sync_channel(2);
        self.data = Some(CaptureData {
            sender,
            receiver,
            user_data: Some(Box::new(user_data)),
            receive_callback,
        });
    }
    fn is_ready(&self) -> bool {
        self.data.is_some()
    }
    fn supports_dmbuf(&self) -> bool {
        true
    }
    fn receive(&mut self) -> Option<R> {
        if let Some(data) = self.data.as_ref() {
            data.receiver.try_iter().last()
        } else {
            None
        }
    }
    fn pause(&mut self) {}
    fn resume(&mut self) {
        if self.data.is_none() {
            return;
        }
        self.receive(); // clear old frames
        self.request_new_frame();
    }
    fn request_new_frame(&mut self) {
        if let Some(handle) = self.handle.take() {
            if handle.is_finished() {
                let (wl, u, session) = handle.join().unwrap(); // safe to unwrap because is_finished
                self.wl = Some(wl);
                self.session = Some(session);
                self.data.as_mut().unwrap().user_data = Some(u);
            } else {
                self.handle = Some(handle);
                return;
            }
        }

        let (Some(wl), Some(session)) = (self.wl.take(), self.session.take()) else {
            return;
        };

        let data = self
            .data
            .as_mut()
            .expect("must call init once before request_new_frame");

        self.handle = Some(std::thread::spawn({
            let sender = data.sender.clone();
            let user_data = data.user_data.take().unwrap();
            let receive_callback = data.receive_callback;
            let output_id = self.output_id;
            move || capture_frame(wl, output_id, session, sender, user_data, receive_callback)
        }));
    }
}

/// One frame: attach a buffer, capture, wait for ready or failed.
fn capture_frame<U, R>(
    mut client: Box<WlxClient>,
    output_id: u32,
    session: Session,
    sender: SyncSender<R>,
    mut user_data: Box<U>,
    receive_callback: fn(&U, WlxFrame) -> Option<R>,
) -> Worker<U>
where
    U: Any + Send + DmaExporter,
    R: Any + Send,
{
    let Some(output) = client.outputs.get(output_id) else {
        return (client, user_data, session);
    };
    let name = output.name.clone();
    let mut transform = output.transform;

    let constraints = session.info.lock().ok().and_then(|info| {
        (info.done && !info.stopped).then(|| {
            (
                info.size.0,
                info.size.1,
                info.shm_formats.clone(),
                info.dmabuf_formats.clone(),
                !info.dmabuf_failed,
            )
        })
    });
    let Some((width, height, shm_formats, dmabuf_formats, try_dmabuf)) = constraints else {
        return (client, user_data, session);
    };
    if width == 0 || height == 0 {
        return (client, user_data, session);
    }

    let (tx, rx) = mpsc::sync_channel::<FrameEvent>(16);
    let frame = session.session.create_frame(&client.queue_handle, tx);

    let mut frame_buffer: Option<(WlxFrame, BufData)> = None;

    if try_dmabuf && let Some(zwp_linux_dmabuf) = client.maybe_zwp_linux_dmabuf.as_ref() {
        // the first of our formats the compositor allows, with a modifier it allows
        for fourcc in FORMATS {
            let Some((_, modifiers)) = dmabuf_formats
                .iter()
                .find(|(code, _)| *code == fourcc as u32)
            else {
                continue;
            };
            let Some((plane, modifier)) = user_data.next_frame(width, height, fourcc) else {
                break;
            };
            if !modifiers.is_empty() && !modifiers.contains(&u64::from(modifier)) {
                log::debug!(
                    "{name}: image copy capture: modifier {:#x} not allowed for {fourcc:?}",
                    u64::from(modifier)
                );
                break;
            }
            let mod_hi = (u64::from(modifier) >> 32) as _;
            let mod_lo = (u64::from(modifier) & 0xFFFFFFFF) as _;
            let fd = unsafe { BorrowedFd::borrow_raw(plane.fd.unwrap()) };

            let params = zwp_linux_dmabuf.create_params(&client.queue_handle, ());
            params.add(fd, 0, plane.offset, plane.stride as _, mod_hi, mod_lo);
            let wl_buffer = params.create_immed(
                width as _,
                height as _,
                fourcc as _,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &client.queue_handle,
                (),
            );
            params.destroy();

            log::trace!("{name}: image copy capture with dma-buf");
            frame.attach_buffer(&wl_buffer);
            frame_buffer = Some((WlxFrame::Implicit(transform), BufData::Dma { wl_buffer }));
            break;
        }
    }

    if frame_buffer.is_none()
        && let Some((fourcc, shm_format)) = FORMATS.iter().find_map(|fourcc| {
            let shm_format = shm_from_fourcc(*fourcc)?;
            shm_formats
                .contains(&shm_format)
                .then_some((*fourcc, shm_format))
        })
    {
        let stride = width * 4;
        let size = stride * height;
        let fd_num = FD_COUNTER.fetch_add(1, Ordering::Relaxed);
        let shm_name = CString::new(format!("wlx-ext-{fd_num}")).unwrap(); // safe
        let fd = unsafe {
            let fd = libc::shm_open(shm_name.as_ptr(), O_CREAT | O_RDWR, S_IRUSR | S_IWUSR);
            libc::shm_unlink(shm_name.as_ptr());
            libc::ftruncate(fd, size as _);
            fd
        };
        let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let wl_pool = client
            .wl_shm
            .create_pool(borrowed_fd, size as _, &client.queue_handle, ());
        let wl_buffer = wl_pool.create_buffer(
            0,
            width as _,
            height as _,
            stride as _,
            shm_format,
            &client.queue_handle,
            (),
        );

        log::trace!("{name}: image copy capture with shared memory");
        frame.attach_buffer(&wl_buffer);
        let mem = MemFdFrame {
            format: FrameFormat {
                width,
                height,
                drm_format: DrmFormat {
                    code: fourcc,
                    modifier: DrmModifier::Invalid,
                },
                transform,
            },
            plane: FramePlane {
                fd: Some(fd),
                offset: 0,
                stride: stride as _,
            },
            mouse: None,
        };
        frame_buffer = Some((
            WlxFrame::MemFd(mem),
            BufData::Shm {
                wl_buffer,
                wl_pool,
                fd,
            },
        ));
    }

    let Some((mut wlx_frame, buffer)) = frame_buffer else {
        log::error!("{name}: image copy capture: no usable buffer format");
        frame.destroy();
        let _ = client.connection.flush();
        return (client, user_data, session);
    };

    frame.damage_buffer(0, 0, width as _, height as _);
    frame.capture();
    let _ = client.connection.flush();

    // the compositor waits for the screen to change before it copies
    'receiver: loop {
        client.dispatch();
        for event in rx.try_iter() {
            match event {
                FrameEvent::Transform(t) => transform = t,
                FrameEvent::Ready => {
                    match &mut wlx_frame {
                        WlxFrame::Implicit(t) => *t = transform,
                        WlxFrame::MemFd(mem) => mem.format.transform = transform,
                        _ => {}
                    }
                    if let Some(r) = receive_callback(&user_data, wlx_frame) {
                        let _ = sender.send(r);
                        log::trace!("{name}: frame ready");
                    }
                    break 'receiver;
                }
                FrameEvent::Failed(reason) => {
                    if reason == Some(FailureReason::BufferConstraints)
                        && matches!(buffer, BufData::Dma { .. })
                        && let Ok(mut info) = session.info.lock()
                    {
                        log::warn!(
                            "{name}: image copy capture refused the dma-buf, using shared memory"
                        );
                        info.dmabuf_failed = true;
                    } else {
                        log::debug!("{name}: image copy capture frame failed: {reason:?}");
                    }
                    break 'receiver;
                }
            }
        }
        if session.info.lock().is_ok_and(|info| info.stopped) {
            break 'receiver;
        }
    }

    frame.destroy();
    drop(buffer);
    let _ = client.connection.flush();

    (client, user_data, session)
}

static FD_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn shm_from_fourcc(fourcc: DrmFourcc) -> Option<Format> {
    match fourcc {
        DrmFourcc::Argb8888 => Some(Format::Argb8888),
        DrmFourcc::Xrgb8888 => Some(Format::Xrgb8888),
        DrmFourcc::Abgr8888 => Some(Format::Abgr8888),
        DrmFourcc::Xbgr8888 => Some(Format::Xbgr8888),
        _ => None,
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, SessionState> for WlxClient {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: <ExtImageCopyCaptureSessionV1 as Proxy>::Event,
        data: &SessionState,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let Ok(mut info) = data.lock() else {
            return;
        };
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                info.updating();
                info.size = (width, height);
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                info.updating();
                if let WEnum::Value(format) = format {
                    info.shm_formats.push(format);
                }
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                info.updating();
                let modifiers = modifiers
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|m| u64::from_ne_bytes(*m))
                    .collect();
                info.dmabuf_formats.push((format, modifiers));
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                info.done = true;
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                info.stopped = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, SyncSender<FrameEvent>> for WlxClient {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureFrameV1,
        event: <ExtImageCopyCaptureFrameV1 as Proxy>::Event,
        data: &SyncSender<FrameEvent>,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Transform {
                transform: WEnum::Value(transform),
            } => {
                let _ = data.send(FrameEvent::Transform(wl_transform_to_frame_transform(
                    transform,
                )));
            }
            ext_image_copy_capture_frame_v1::Event::Ready => {
                let _ = data.send(FrameEvent::Ready);
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                let reason = match reason {
                    WEnum::Value(reason) => Some(reason),
                    WEnum::Unknown(_) => None,
                };
                let _ = data.send(FrameEvent::Failed(reason));
            }
            _ => {}
        }
    }
}
