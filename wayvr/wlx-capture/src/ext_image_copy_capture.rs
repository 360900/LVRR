//! LVRR: ext-image-copy-capture-v1, the standard successor of wlr-screencopy (wlroots 0.19,
//! Hyprland, niri, COSMIC). One session per output; for every frame the client attaches a
//! buffer of its own and the compositor copies the screen into it once it changed.

use std::{
    any::Any,
    fs::{self, File},
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use drm_fourcc::{DrmFormat, DrmFourcc, DrmModifier};
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
    wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    },
};

use crate::{
    WlxCapture,
    frame::{self, DmaExporter, FrameFormat, FramePlane, MemFdFrame, NoDmabuf, WlxFrame},
    wayland::{WlxClient, wl_transform_to_frame_transform},
};

const CONSTRAINTS_TIMEOUT: Duration = Duration::from_secs(5);
/// Hyprland fails a frame before it sends the constraints that made it fail
const NEW_CONSTRAINTS_TIMEOUT: Duration = Duration::from_secs(1);
/// how long a worker waits on the compositor before it checks whether its capture was dropped
const WAIT_SLICE: Duration = Duration::from_millis(250);
/// the wait before the next try after a failed frame, doubling up to the max
const RETRY_DELAY_MIN: Duration = Duration::from_millis(50);
const RETRY_DELAY_MAX: Duration = Duration::from_secs(1);
/// dma-buf failures in a row before shared memory takes over
const DMABUF_TRIES: u8 = 3;

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
    /// the device dma-bufs have to be allocated on
    dmabuf_device: Option<libc::dev_t>,
    /// the last batch of constraints ended
    done: bool,
    /// counts the batches that ended, so a failed frame can tell whether new ones came
    generation: u64,
    stopped: bool,
    /// our dma-buf kept failing, stay with shared memory
    dmabuf_failed: bool,
    /// failures in a row: frames the compositor failed with our dma-buf, dma-bufs we couldn't make
    dmabuf_failures: u8,
    /// dmabuf_device is another GPU than the exporter's, worked out once per batch
    other_gpu: Option<bool>,
}

impl SessionInfo {
    /// A constraint event after `done` starts a new batch, which may suit a dma-buf again
    fn updating(&mut self) {
        if self.done {
            self.done = false;
            self.shm_formats.clear();
            self.dmabuf_formats.clear();
            self.dmabuf_device = None;
            self.other_gpu = None;
            self.dmabuf_failed = false;
            self.dmabuf_failures = 0;
        }
    }

    /// Counts a dma-buf failure, true for the one that makes shared memory take over
    fn dmabuf_failure(&mut self) -> bool {
        self.dmabuf_failures = self.dmabuf_failures.saturating_add(1);
        let last = self.dmabuf_failures == DMABUF_TRIES;
        self.dmabuf_failed |= last;
        last
    }
}

type SessionState = Arc<Mutex<SessionInfo>>;

/// The constraints one try of a frame allocates for
struct Constraints {
    generation: u64,
    size: (u32, u32),
    shm_formats: Vec<Format>,
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
    try_dmabuf: bool,
    /// the compositor takes dma-bufs only from another GPU than the exporter's
    other_gpu: bool,
}

struct Session {
    source: ExtImageCaptureSourceV1,
    session: ExtImageCopyCaptureSessionV1,
    info: SessionState,
    buffers: Buffers,
    /// the caller takes dma-bufs: init got formats for them
    dmabuf: bool,
}

impl Session {
    /// The constraints of the last complete batch, None while a batch is coming in. `node` is
    /// the render node the exporter allocates dma-bufs on.
    fn constraints(&self, node: Option<(u32, u32)>) -> Option<Constraints> {
        let mut info = self.info.lock().ok()?;
        if !info.done {
            return None;
        }
        // dma-bufs have to come from the device the compositor names, wlroots ends a client
        // whose dma-buf fails to import. Unknown counts as the same GPU, so the dma-buf is tried.
        let device = info.dmabuf_device;
        let other_gpu = self.dmabuf
            && *info
                .other_gpu
                .get_or_insert_with(|| same_gpu(device, node) == Some(false));
        Some(Constraints {
            generation: info.generation,
            size: info.size,
            shm_formats: info.shm_formats.clone(),
            dmabuf_formats: info.dmabuf_formats.clone(),
            try_dmabuf: self.dmabuf && !info.dmabuf_failed && !other_gpu,
            other_gpu,
        })
    }

    /// Stopped by the compositor, or ended by a failed connection or a lost output
    fn stopped(&self) -> bool {
        self.info.lock().map_or(true, |info| info.stopped)
    }

    fn stop(&self) {
        if let Ok(mut info) = self.info.lock() {
            info.stopped = true;
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // the buffers before the session that copied into them
        self.buffers.shm = None;
        self.buffers.clear_dma();
        self.session.destroy();
        self.source.destroy();
    }
}

/// The buffers a session copies into, kept from frame to frame: a new buffer makes the
/// compositor map its pool or import its dma-buf again
#[derive(Default)]
struct Buffers {
    shm: Option<ShmBuffer>,
    /// a wl_buffer per exported image, oldest first: the exporter takes turns with two
    dma: Vec<(DmaKey, WlBuffer)>,
    /// the batch of constraints the dma-buf buffers were made under
    dma_generation: u64,
    /// why that batch gets shared memory was logged
    shm_logged: bool,
}

impl Buffers {
    /// The wl_buffer for an exported image, made the first time the image is used
    fn dma_buffer(
        &mut self,
        client: &WlxClient,
        zwp_linux_dmabuf: &ZwpLinuxDmabufV1,
        key: DmaKey,
        fd: BorrowedFd,
    ) -> WlBuffer {
        if let Some((_, wl_buffer)) = self.dma.iter().find(|(known, _)| *known == key) {
            return wl_buffer.clone();
        }
        // the exporter takes turns with two images, a new one replaced the oldest
        if self.dma.len() == 2 {
            self.dma.remove(0).1.destroy();
        }

        let mod_hi = (key.modifier >> 32) as _;
        let mod_lo = (key.modifier & 0xFFFFFFFF) as _;
        let params = zwp_linux_dmabuf.create_params(&client.queue_handle, ());
        params.add(fd, 0, key.offset, key.stride as _, mod_hi, mod_lo);
        let wl_buffer = params.create_immed(
            key.width as _,
            key.height as _,
            key.fourcc as _,
            zwp_linux_buffer_params_v1::Flags::empty(),
            &client.queue_handle,
            (),
        );
        params.destroy();
        self.dma.push((key, wl_buffer.clone()));
        wl_buffer
    }

    fn clear_dma(&mut self) {
        for (_, wl_buffer) in self.dma.drain(..) {
            wl_buffer.destroy();
        }
    }
}

/// A memfd the compositor copies into, with its pool and a buffer over all of it
struct ShmBuffer {
    fd: OwnedFd,
    pool: WlShmPool,
    buffer: WlBuffer,
    width: u32,
    height: u32,
    fourcc: DrmFourcc,
}

impl ShmBuffer {
    fn new(
        client: &WlxClient,
        width: u32,
        height: u32,
        fourcc: DrmFourcc,
        format: Format,
    ) -> io::Result<Self> {
        let stride = width * 4;
        let size = stride * height;
        // a memfd has no name in /dev/shm that another user could open or create first
        // SAFETY: the name is a C string
        let fd = unsafe { libc::memfd_create(c"wlx-ext".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: memfd_create made the fd, nothing else owns it
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(size.into())?;
        let fd = OwnedFd::from(file);

        let pool = client
            .wl_shm
            .create_pool(fd.as_fd(), size as _, &client.queue_handle, ());
        let buffer = pool.create_buffer(
            0,
            width as _,
            height as _,
            stride as _,
            format,
            &client.queue_handle,
            (),
        );
        Ok(Self {
            fd,
            pool,
            buffer,
            width,
            height,
            fourcc,
        })
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

/// The exported image a dma-buf wl_buffer was made for
#[derive(PartialEq)]
struct DmaKey {
    /// the dma-buf's device and inode: a freed image's fd number can come back, its inode can't
    dev: libc::dev_t,
    ino: libc::ino_t,
    width: u32,
    height: u32,
    fourcc: DrmFourcc,
    modifier: u64,
    offset: u32,
    stride: i32,
}

impl DmaKey {
    fn new(
        fd: BorrowedFd,
        plane: &FramePlane,
        width: u32,
        height: u32,
        fourcc: DrmFourcc,
        modifier: u64,
    ) -> io::Result<Self> {
        // SAFETY: stat is plain data, all zeroes is a valid value
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fd is open and stat is valid for writes
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            dev: stat.st_dev,
            ino: stat.st_ino,
            width,
            height,
            fourcc,
            modifier,
            offset: plane.offset,
            stride: plane.stride,
        })
    }
}

enum FrameEvent {
    Transform(frame::Transform),
    Ready,
    Failed(Option<FailureReason>),
}

enum BufData {
    /// the session's memfd, open while the session keeps the buffer
    Shm {
        fd: RawFd,
    },
    Dma {
        modifier: u64,
    },
}

/// The session's buffer a frame copies into, kept across the tries of a frame
struct Buffer {
    wl_buffer: WlBuffer,
    data: BufData,
    width: u32,
    height: u32,
    fourcc: DrmFourcc,
}

impl Buffer {
    /// Whether the compositor still takes the buffer: a new batch can change the size or the
    /// formats, and a dma-buf that keeps failing gives way to shared memory
    fn fits(&self, constraints: &Constraints) -> bool {
        if (self.width, self.height) != constraints.size {
            return false;
        }
        match &self.data {
            BufData::Dma { modifier, .. } => {
                constraints.try_dmabuf
                    && constraints.dmabuf_formats.iter().any(|(code, modifiers)| {
                        *code == self.fourcc as u32
                            && (modifiers.is_empty() || modifiers.contains(modifier))
                    })
            }
            BufData::Shm { .. } => shm_from_fourcc(self.fourcc)
                .is_some_and(|format| constraints.shm_formats.contains(&format)),
        }
    }

    /// The frame for the receive callback, once the compositor copied into the buffer
    fn frame(&self, transform: frame::Transform) -> WlxFrame {
        match &self.data {
            // the callback takes the exporter's current image
            BufData::Dma { .. } => WlxFrame::Implicit(transform),
            BufData::Shm { fd, .. } => WlxFrame::MemFd(MemFdFrame {
                format: FrameFormat {
                    width: self.width,
                    height: self.height,
                    drm_format: DrmFormat {
                        code: self.fourcc,
                        modifier: DrmModifier::Invalid,
                    },
                    transform,
                },
                plane: FramePlane {
                    fd: Some(*fd),
                    offset: 0,
                    stride: (self.width * 4) as _,
                },
                mouse: None,
            }),
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
    name: Arc<str>,
    wl: Option<Box<WlxClient>>,
    session: Option<Session>,
    handle: Option<JoinHandle<Worker<U>>>,
    data: Option<CaptureData<U, R>>,
    /// the frame copied while the screen was hidden, kept by resume() for the next receive()
    pending: Option<R>,
    /// a request came while the worker was still running, receive() makes it once it ended
    owed: bool,
    /// set on drop, ends a worker still waiting on the compositor
    cancel: Arc<AtomicBool>,
}

impl<U, R> Drop for ExtImageCopyCapture<U, R> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
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
            buffers: Buffers::default(),
            dmabuf: false,
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
            if let Err(e) = client.try_dispatch_pending() {
                log::warn!("{name}: image copy capture: the Wayland connection failed: {e}");
                return None;
            }
        }

        Some(Self {
            output_id,
            name,
            wl: Some(client),
            session: Some(session),
            handle: None,
            data: None,
            pending: None,
            owed: false,
            cancel: Arc::default(),
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
        dmabuf_formats: &[DrmFormat],
        user_data: U,
        receive_callback: fn(&U, WlxFrame) -> Option<R>,
    ) {
        debug_assert!(self.wl.is_some());

        // no formats: the caller takes shared memory only, as with CPU capture
        if let Some(session) = self.session.as_mut() {
            session.dmabuf = !dmabuf_formats.is_empty();
        }

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
        // checked before the channel: once the worker ended, its frame is in there, so no new
        // worker flips the exporter's images while that frame waits to be shown
        let ended = self.handle.as_ref().is_some_and(JoinHandle::is_finished);
        let newest = self.data.as_ref()?.receiver.try_iter().last();
        // a frame kept by resume() is older than any in the channel
        let kept = self.pending.take();
        let frame = newest.or(kept);
        if frame.is_some() {
            // the screen asks for the next frame after showing this one, see resume()
            self.owed = false;
        } else if self.owed && ended {
            // the screen asks again only after a new frame
            self.request_new_frame();
        }
        frame
    }
    fn pause(&mut self) {}
    fn resume(&mut self) {
        if self.data.is_none() {
            return;
        }
        // the frame copied while hidden is the newest, and the next capture waits for the
        // output to change again
        self.pending = self.receive();
        // the screen asks for the next frame after showing the kept one: a worker started now
        // would call next_frame(), and a callback that runs at receive time would then take the
        // dma-buf that is on screen
        if self.pending.is_none() {
            self.request_new_frame();
        }
    }
    fn request_new_frame(&mut self) {
        self.owed = false;
        if let Some(handle) = self.handle.take() {
            if handle.is_finished() {
                match handle.join() {
                    Ok((wl, u, session)) => {
                        self.wl = Some(wl);
                        self.session = Some(session);
                        self.data.as_mut().unwrap().user_data = Some(u);
                    }
                    // wl and session stay None: the screen keeps its last frame
                    Err(_) => log::error!(
                        "{}: image copy capture: the worker panicked, no more frames",
                        self.name
                    ),
                }
            } else {
                // it may have sent its frame and not ended yet: receive() asks again once it did
                self.handle = Some(handle);
                self.owed = true;
                return;
            }
        }

        // a stopped session or a failed connection makes no more frames
        if self.session.as_ref().is_none_or(Session::stopped) {
            return;
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
            let cancel = self.cancel.clone();
            move || {
                capture_frame(
                    wl,
                    output_id,
                    session,
                    cancel,
                    sender,
                    user_data,
                    receive_callback,
                )
            }
        }));
    }
}

/// Captures a frame into a buffer of ours and sends it. A failed copy is tried again, so the
/// worker ends only once it sent a frame, the session stopped, the connection failed or the
/// capture was dropped.
fn capture_frame<U, R>(
    mut client: Box<WlxClient>,
    output_id: u32,
    mut session: Session,
    cancel: Arc<AtomicBool>,
    sender: SyncSender<R>,
    mut user_data: Box<U>,
    receive_callback: fn(&U, WlxFrame) -> Option<R>,
) -> Worker<U>
where
    U: Any + Send + DmaExporter,
    R: Any + Send,
{
    let Some(name) = client
        .outputs
        .get(output_id)
        .map(|output| output.name.clone())
    else {
        session.stop();
        return (client, user_data, session);
    };
    // kept across tries: another next_frame() would hand out the dma-buf that is on screen
    let mut buffer: Option<Buffer> = None;
    let mut delay = RETRY_DELAY_MIN;
    let node = user_data.drm_render_node();

    loop {
        // new constraints may have come since the last frame, or be half read
        let done = || session.info.lock().is_ok_and(|info| info.done);
        let deadline = Instant::now() + CONSTRAINTS_TIMEOUT;
        if !wait_for(&mut client, &session, &cancel, &name, Some(deadline), done) {
            break;
        }
        let Some(constraints) = session.constraints(node) else {
            log::warn!(
                "{name}: image copy capture: no buffer constraints in {}s",
                CONSTRAINTS_TIMEOUT.as_secs()
            );
            if !wait_for(&mut client, &session, &cancel, &name, None, done) {
                break;
            }
            continue;
        };
        let Some(output) = client.outputs.get(output_id) else {
            session.stop();
            break;
        };
        let mut transform = output.transform;
        let new_batch = || {
            session
                .info
                .lock()
                .is_ok_and(|info| info.generation != constraints.generation)
        };

        if buffer
            .as_ref()
            .is_some_and(|buffer| !buffer.fits(&constraints))
        {
            buffer = None;
        }
        let (width, height) = constraints.size;
        if buffer.is_none() && width > 0 && height > 0 {
            let (buffers, info) = (&mut session.buffers, &session.info);
            match buffer_for(
                &client,
                buffers,
                info,
                user_data.as_mut(),
                &constraints,
                &name,
            ) {
                Ok(new) => buffer = new,
                Err(e) => {
                    log::warn!("{name}: image copy capture: no shared memory buffer: {e}");
                    if !back_off(&mut client, &session, &cancel, &name, &mut delay) {
                        break;
                    }
                    continue;
                }
            }
        }
        let Some(buf) = buffer.as_ref() else {
            if width == 0 || height == 0 {
                log::debug!("{name}: image copy capture: no buffer size");
            } else {
                log::error!("{name}: image copy capture: no usable buffer format");
            }
            // nothing to capture into until the compositor sends other constraints
            if !wait_for(&mut client, &session, &cancel, &name, None, new_batch) {
                break;
            }
            continue;
        };

        let (tx, rx) = mpsc::sync_channel::<FrameEvent>(16);
        let frame = session.session.create_frame(&client.queue_handle, tx);
        frame.attach_buffer(&buf.wl_buffer);
        frame.damage_buffer(0, 0, width as _, height as _);
        frame.capture();

        // the compositor waits for the screen to change before it copies
        let mut outcome = None;
        let go_on = wait_for(&mut client, &session, &cancel, &name, None, || {
            for event in rx.try_iter() {
                match event {
                    FrameEvent::Transform(t) => transform = t,
                    event => outcome = Some(event),
                }
            }
            outcome.is_some()
        });
        // one frame per session: the next try creates another
        frame.destroy();
        if !go_on {
            break;
        }

        let reason = match outcome {
            Some(FrameEvent::Ready) => {
                if let Some(r) = receive_callback(&user_data, buf.frame(transform)) {
                    // shm frames come between failed exports: only a dma-buf frame ends the run
                    if matches!(buf.data, BufData::Dma { .. })
                        && let Ok(mut info) = session.info.lock()
                    {
                        info.dmabuf_failures = 0;
                    }
                    let _ = sender.send(r);
                    log::trace!("{name}: frame ready");
                    break;
                }
                // the callback logged why, try again as after an unknown failure
                None
            }
            Some(FrameEvent::Failed(reason)) => {
                log::debug!("{name}: image copy capture frame failed: {reason:?}");
                reason
            }
            _ => None,
        };
        match reason {
            Some(FailureReason::Stopped) => {
                session.stop();
                break;
            }
            Some(FailureReason::BufferConstraints) => {
                // wlroots and COSMIC send the new constraints before the failure, Hyprland after
                let deadline = Instant::now() + NEW_CONSTRAINTS_TIMEOUT;
                if !wait_for(
                    &mut client,
                    &session,
                    &cancel,
                    &name,
                    Some(deadline),
                    new_batch,
                ) {
                    break;
                }
                if new_batch() {
                    continue;
                }
                // still the same constraints: back off, so a lasting mismatch doesn't spin
            }
            _ => {}
        }

        // compositors report a dma-buf they can't render into as an unknown failure
        if matches!(buf.data, BufData::Dma { .. })
            && session
                .info
                .lock()
                .is_ok_and(|mut info| info.dmabuf_failure())
        {
            log::warn!(
                "{name}: image copy capture: the compositor can't use the dma-buf, using shared memory"
            );
        }
        if !back_off(&mut client, &session, &cancel, &name, &mut delay) {
            break;
        }
    }

    let _ = client.connection.flush();

    (client, user_data, session)
}

/// Waits before the next try, twice as long as the last time up to RETRY_DELAY_MAX. False when
/// the worker has to end, as for wait_for.
fn back_off(
    client: &mut WlxClient,
    session: &Session,
    cancel: &AtomicBool,
    name: &str,
    delay: &mut Duration,
) -> bool {
    let deadline = Instant::now() + *delay;
    *delay = (*delay * 2).min(RETRY_DELAY_MAX);
    wait_for(client, session, cancel, name, Some(deadline), || false)
}

/// Dispatches the compositor's events until `until` holds or the deadline passes, waking at
/// least every WAIT_SLICE to see whether the capture was dropped. False when the worker has to
/// end: the capture was dropped, the session stopped or the connection failed.
fn wait_for(
    client: &mut WlxClient,
    session: &Session,
    cancel: &AtomicBool,
    name: &str,
    deadline: Option<Instant>,
    mut until: impl FnMut() -> bool,
) -> bool {
    loop {
        // requests left queued, e.g. by a full socket, go out before the wait
        let _ = client.connection.flush();
        if let Err(e) = client.try_dispatch_pending() {
            log::warn!("{name}: image copy capture: the Wayland connection failed: {e}");
            session.stop();
            return false;
        }
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        if session.stopped() {
            log::info!("{name}: image copy capture: the compositor stopped the session");
            return false;
        }
        if until() {
            return true;
        }
        let now = Instant::now();
        if deadline.is_some_and(|deadline| now >= deadline) {
            return true;
        }
        let slice = now + WAIT_SLICE;
        client.wait_readable(deadline.map_or(slice, |deadline| deadline.min(slice)));
    }
}

/// A buffer for the constraints: a dma-buf from the exporter when the compositor allows one of
/// our formats with a modifier the exporter has, from the exporter's GPU, else shared memory.
/// Made once and kept in `buffers` for the next frames. A dma-buf that can't be made counts
/// as a failure in `info`. None when the compositor allows none of our formats, Err when no
/// shared memory could be made.
fn buffer_for<U: DmaExporter>(
    client: &WlxClient,
    buffers: &mut Buffers,
    info: &SessionState,
    user_data: &mut U,
    constraints: &Constraints,
    name: &str,
) -> io::Result<Option<Buffer>> {
    let (width, height) = constraints.size;

    // a new batch can come with other modifiers or another device: import the images again
    if buffers.dma_generation != constraints.generation {
        buffers.dma_generation = constraints.generation;
        buffers.clear_dma();
        buffers.shm_logged = false;
    }

    if constraints.try_dmabuf
        && let Some(zwp_linux_dmabuf) = client.maybe_zwp_linux_dmabuf.as_ref()
    {
        // the first of our formats the compositor allows, with a modifier it allows
        let mut failed = false;
        for fourcc in FORMATS {
            let Some((_, modifiers)) = constraints
                .dmabuf_formats
                .iter()
                .find(|(code, _)| *code == fourcc as u32)
            else {
                continue;
            };
            let (plane, modifier) =
                match user_data.next_frame_allowed(width, height, fourcc, modifiers) {
                    Ok(image) => image,
                    // where the exporter has none for one format it may have one for the next
                    Err(NoDmabuf::Modifier) => continue,
                    // the exporter logged why, the next format would fail the same way
                    Err(NoDmabuf::Export) => {
                        failed = true;
                        break;
                    }
                };
            let modifier = u64::from(modifier);
            let Some(fd) = plane.fd else {
                failed = true;
                break;
            };
            // SAFETY: the exporter keeps the image's fd open
            let fd = unsafe { BorrowedFd::borrow_raw(fd) };
            let key = match DmaKey::new(fd, &plane, width, height, fourcc, modifier) {
                Ok(key) => key,
                Err(e) => {
                    log::warn!("{name}: image copy capture: can't stat the dma-buf: {e}");
                    failed = true;
                    break;
                }
            };
            let wl_buffer = buffers.dma_buffer(client, zwp_linux_dmabuf, key, fd);
            // a session copies into one kind of buffer at a time
            buffers.shm = None;

            log::trace!("{name}: image copy capture with dma-buf");
            return Ok(Some(Buffer {
                wl_buffer,
                data: BufData::Dma { modifier },
                width,
                height,
                fourcc,
            }));
        }
        if failed {
            // a lasting failure, e.g. no VRAM left, stops the tries until the next batch
            if info.lock().is_ok_and(|mut info| info.dmabuf_failure()) {
                log::warn!(
                    "{name}: image copy capture: no dma-buf could be exported, using shared memory"
                );
            }
        } else if !buffers.shm_logged {
            buffers.shm_logged = true;
            log::info!(
                "{name}: image copy capture: no dma-buf format and modifier in common with the compositor, using shared memory"
            );
        }
    } else if constraints.other_gpu && !buffers.shm_logged {
        buffers.shm_logged = true;
        log::info!(
            "{name}: image copy capture: the compositor wants dma-bufs from another GPU, using shared memory"
        );
    }

    let Some((fourcc, shm_format)) = FORMATS.iter().find_map(|fourcc| {
        let shm_format = shm_from_fourcc(*fourcc)?;
        constraints
            .shm_formats
            .contains(&shm_format)
            .then_some((*fourcc, shm_format))
    }) else {
        return Ok(None);
    };
    // one kind of buffer at a time, as above
    buffers.clear_dma();
    let shm = match buffers.shm.take() {
        Some(shm) if (shm.width, shm.height, shm.fourcc) == (width, height, fourcc) => shm,
        _ => ShmBuffer::new(client, width, height, fourcc, shm_format)?,
    };

    log::trace!("{name}: image copy capture with shared memory");
    let buffer = Buffer {
        wl_buffer: shm.buffer.clone(),
        data: BufData::Shm {
            fd: shm.fd.as_raw_fd(),
        },
        width,
        height,
        fourcc,
    };
    buffers.shm = Some(shm);
    Ok(Some(buffer))
}

fn shm_from_fourcc(fourcc: DrmFourcc) -> Option<Format> {
    match fourcc {
        DrmFourcc::Argb8888 => Some(Format::Argb8888),
        DrmFourcc::Xrgb8888 => Some(Format::Xrgb8888),
        DrmFourcc::Abgr8888 => Some(Format::Abgr8888),
        DrmFourcc::Xbgr8888 => Some(Format::Xbgr8888),
        _ => None,
    }
}

/// Whether the device the compositor names and the exporter's render node are one GPU, None
/// when either is unknown
fn same_gpu(device: Option<libc::dev_t>, node: Option<(u32, u32)>) -> Option<bool> {
    let (device, node) = (device?, node?);
    // the compositor may name a GPU's primary node, which has another dev_t than its render
    // node: the device behind both tells the GPU
    let gpu = |(major, minor): (u32, u32)| {
        fs::canonicalize(format!("/sys/dev/char/{major}:{minor}/device")).ok()
    };
    Some(gpu((libc::major(device), libc::minor(device)))? == gpu(node)?)
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
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                info.updating();
                // a dev_t in the compositor's byte order, which is ours
                info.dmabuf_device = device
                    .as_slice()
                    .try_into()
                    .ok()
                    .map(libc::dev_t::from_ne_bytes);
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                info.done = true;
                info.generation += 1;
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
