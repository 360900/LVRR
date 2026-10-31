//! LVRR: a compositor hands out DMA-buf frames that it may still be drawing, and draws into
//! them again as soon as the capture gives them back. Reading them later, as the render thread
//! does, mixes frames during motion, most visibly when the compositor draws on another GPU.
//!
//! So each frame is copied into an image of our own while the capture still holds the buffer,
//! after the compositor's writes have finished. This runs on the capture thread and submits on
//! the second graphics queue, so it needs a GPU that has one: with a single queue, captures
//! give the buffer back before the render thread sees the frame (see `MainThreadWlxCapture`).

use std::{sync::Arc, time::Duration, time::Instant};

use wgui::gfx::{CommandBufferUsage, Image, ImageUsage};
use wlx_capture::frame::DmabufFrame;

use super::{CAPTURE_QUEUE_MUTEX, WlxCaptureIn};
use crate::graphics::dmabuf::WGfxDmabuf;

/// Longest wait for the compositor to finish drawing a frame
const WRITE_TIMEOUT: Duration = Duration::from_millis(100);

/// The frame as an image the compositor can't change. When the driver can't copy from this
/// format and modifier, the frame is used in place, as before.
pub(super) fn frame_image(me: &WlxCaptureIn, frame: DmabufFrame) -> anyhow::Result<Arc<Image>> {
    wait_for_writes(me, &frame);
    // the fds stay owned by the capture; the import duplicates them
    let in_place = DmabufFrame {
        format: frame.format,
        num_planes: frame.num_planes,
        planes: frame.planes,
        mouse: frame.mouse.clone(),
    };
    match me
        .gfx
        .dmabuf_texture_with_usage(frame, ImageUsage::SAMPLED | ImageUsage::TRANSFER_SRC)
    {
        Ok(imported) => copy(me, &imported),
        Err(e) => {
            log::debug!("{}: DMA-buf frame used in place: {e:?}", me.name);
            me.gfx.dmabuf_texture(in_place)
        }
    }
}

fn copy(me: &WlxCaptureIn, imported: &Arc<Image>) -> anyhow::Result<Arc<Image>> {
    let [width, height, _] = imported.extent();
    let image = me.gfx.new_image(
        width,
        height,
        imported.format(),
        ImageUsage::TRANSFER_DST | ImageUsage::TRANSFER_SRC | ImageUsage::SAMPLED,
    )?;
    let mut cmd = me
        .gfx
        .create_capture_command_buffer(CommandBufferUsage::OneTimeSubmit)?;
    cmd.copy_image(imported, [0, 0, 0], &image, [0, 0, 0], None)?;
    // waits until the copy is done, so the buffer goes back only afterwards
    let _guard = CAPTURE_QUEUE_MUTEX.lock().unwrap(); // mutex poison only
    cmd.build_and_execute_now()?;
    Ok(image)
}

/// A DMA-buf polls readable once its write fences have signaled. This also works when the
/// compositor draws on another GPU, which the reading GPU's driver does not wait for.
fn wait_for_writes(me: &WlxCaptureIn, frame: &DmabufFrame) {
    let mut fds: Vec<libc::pollfd> = frame.planes[..frame.num_planes]
        .iter()
        .filter_map(|p| p.fd)
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    fds.dedup_by_key(|p| p.fd);
    let deadline = Instant::now() + WRITE_TIMEOUT;
    while !fds.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        // SAFETY: fds is a valid array of fds.len() pollfd entries
        let ready = unsafe {
            libc::poll(
                fds.as_mut_ptr(),
                fds.len() as libc::nfds_t,
                left.as_millis() as i32,
            )
        };
        if ready == 0 {
            log::debug!(
                "{}: DMA-buf frame still being drawn after {WRITE_TIMEOUT:?}",
                me.name
            );
            return;
        }
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        fds.retain(|p| p.revents == 0);
    }
}
