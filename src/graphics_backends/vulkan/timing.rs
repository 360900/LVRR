//! When the GPU finished the work an app submitted for each frame, from a fence submitted to the
//! app's queue after it. A thread waits for the fences, so the times are on the CPU's clock, which
//! the app's device has no common timestamps with unless it enabled VK_KHR_calibrated_timestamps,
//! and the app never waits for them.

use crate::compositor::GpuDone;
use ash::vk;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

/// With this many frames pending, the GPU is far behind and the next ones go unmeasured.
const MAX_FENCES: usize = 4;

/// How long the thread waits for a fence at a time, to notice when it should stop.
const WAIT_SLICE_NS: u64 = 50_000_000;

pub(super) struct GpuTimer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

struct Shared {
    device: ash::Device,
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    /// Fences the GPU signaled. The app's thread resets them before submitting them again, so
    /// the timing thread never uses the device after its fences signaled, which the app may
    /// destroy it after.
    signaled: Vec<vk::Fence>,
    /// Submitted fences and their frames, oldest first.
    pending: VecDeque<(vk::Fence, GpuDone)>,
    stop: bool,
    /// Something failed, e.g. the device was lost. Nothing is measured anymore.
    broken: bool,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for the oldest pending fence and reports its frame. False once the timer stops.
    fn wait_next(&self) -> bool {
        let fence = {
            let mut state = self.state();
            loop {
                if state.stop || state.broken {
                    return false;
                }
                if let Some((fence, _)) = state.pending.front() {
                    break *fence;
                }
                state = self
                    .wake
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        // Waiting doesn't need the fence to be externally synchronized, unlike resetting it.
        match unsafe { self.device.wait_for_fences(&[fence], true, WAIT_SLICE_NS) } {
            Ok(()) => {
                let done = Instant::now();
                let frame = {
                    let mut state = self.state();
                    // Only this thread takes pending fences.
                    let Some((fence, frame)) = state.pending.pop_front() else {
                        return true;
                    };
                    state.signaled.push(fence);
                    frame
                };
                frame.report(done);
                true
            }
            Err(vk::Result::TIMEOUT) => true,
            Err(e) => {
                crate::warn_once!("Failed to wait for the GPU to finish a frame: {}", e);
                self.state().broken = true;
                false
            }
        }
    }
}

impl GpuTimer {
    pub fn new(device: ash::Device) -> Self {
        let mut timer = Self::without_thread(device);
        let shared = timer.shared.clone();
        let thread = std::thread::Builder::new()
            .name("xrizer-gpu-timing".into())
            .spawn(move || while shared.wait_next() {});
        match thread {
            Ok(thread) => timer.thread = Some(thread),
            Err(e) => {
                crate::warn_once!("Failed to start the GPU timing thread: {}", e);
                timer.shared.state().broken = true;
            }
        }
        timer
    }

    /// Without the thread, tests wait for the fences themselves with [Self::wait_next].
    pub fn without_thread(device: ash::Device) -> Self {
        Self {
            shared: Arc::new(Shared {
                device,
                state: Mutex::default(),
                wake: Condvar::new(),
            }),
            thread: None,
        }
    }

    #[cfg(test)]
    pub fn wait_next(&self) -> bool {
        self.shared.wait_next()
    }

    /// Submits a fence after the work on `queue`, which the app hands us during Submit, and
    /// reports to `done` when it signals.
    pub fn frame_submitted(&mut self, queue: vk::Queue, done: GpuDone) {
        let device = &self.shared.device;
        let reused = {
            let state = &mut *self.shared.state();
            if state.broken || (state.signaled.is_empty() && state.pending.len() >= MAX_FENCES) {
                return;
            }
            state.signaled.pop()
        };
        let fence = match reused {
            Some(fence) => unsafe { device.reset_fences(&[fence]) }.map(|()| fence),
            None => unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) },
        };
        let fence = match fence {
            Ok(fence) => fence,
            Err(e) => {
                crate::warn_once!("Failed to set up a fence for frame timing: {}", e);
                let mut state = self.shared.state();
                state.broken = true;
                state.signaled.extend(reused);
                return;
            }
        };
        // An empty batch signals its fence once the work submitted before it is done.
        if let Err(e) = unsafe { device.queue_submit(queue, &[], fence) } {
            crate::warn_once!("Failed to submit a fence for frame timing: {}", e);
            let mut state = self.shared.state();
            state.broken = true;
            state.signaled.push(fence);
            return;
        }
        self.shared.state().pending.push_back((fence, done));
        self.shared.wake.notify_one();
    }
}

impl Drop for GpuTimer {
    fn drop(&mut self) {
        self.shared.state().stop = true;
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let state = std::mem::take(&mut *self.shared.state());
        let device = &self.shared.device;
        let mut pending: Vec<vk::Fence> = state.pending.iter().map(|(fence, _)| *fence).collect();
        // Fences the GPU may still signal are leaked rather than destroyed while in use.
        if !pending.is_empty()
            && unsafe { device.wait_for_fences(&pending, true, super::COPY_TIMEOUT_NS) }.is_err()
        {
            pending.clear();
        }
        for fence in state.signaled.into_iter().chain(pending) {
            unsafe { device.destroy_fence(fence, None) };
        }
    }
}
