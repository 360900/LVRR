//! Frame timing for GetFrameTiming(s), GetCumulativeStats and GetFrameTimeRemaining.
//!
//! The CPU phases are measured around the frame loop's calls (WaitGetPoses, xrWaitFrame, Submit,
//! xrEndFrame), and the GPU's from when the backend reports the GPU finished a frame's work.
//! OpenXR doesn't say when a frame was shown, so that comes from the display times xrWaitFrame
//! predicts: one that skips vsyncs means the runtime showed the previous frame again for them,
//! reprojected like every frame it composes.

use openvr as vr;
use openxr as xr;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Ended frames kept for GetFrameTiming(s).
const HISTORY: usize = 128;

/// Predictions further off than this are taken as garbage.
const MAX_LEAD: Nanos = 1_000_000_000;

/// Clock offsets kept, see [State::clock_offsets].
const CLOCK_SAMPLES: usize = 16;

/// After this long without a new frame, the app isn't in a frame loop.
const STALE: Nanos = 250_000_000;

/// Nanoseconds since the timer was created.
type Nanos = i64;

/// The timing of the frames the app rendered. The lock is only held to record or read a frame,
/// never while calling the runtime or a graphics API.
pub struct FrameTimer {
    origin: Instant,
    state: Mutex<State>,
}

/// Lets a graphics backend report when the GPU finished the work the app submitted for a frame,
/// up to its last Submit.
pub struct GpuDone(Box<dyn FnOnce(Instant) + Send + Sync>);

impl GpuDone {
    pub fn new(report: impl FnOnce(Instant) + Send + Sync + 'static) -> Self {
        Self(Box::new(report))
    }

    pub fn report(self, done: Instant) {
        (self.0)(done)
    }
}

/// A frame xrWaitFrame returned.
pub struct WaitedFrame {
    pub display_time: xr::Time,
    pub period: i64,
    /// Vsyncs the previous frame was shown again for before this one.
    pub missed: u32,
    pub wait_start: Instant,
    pub wait_end: Instant,
    /// The runtime's current time read right after the instant, if it can tell.
    pub now: Option<(Instant, xr::Time)>,
}

#[derive(Clone, Copy, Default)]
struct Frame {
    /// Counts waited frames, which aren't all ended.
    seq: u64,
    /// m_nFrameIndex, set when the frame ends.
    index: u32,
    /// When the app asked for the frame: WaitGetPoses was called, or else the wait started.
    called: Nanos,
    /// Since the previous frame was asked for.
    interval: Option<Nanos>,
    wait_start: Nanos,
    wait_end: Nanos,
    /// WaitGetPoses returned the frame's poses.
    poses_ready: Option<Nanos>,
    period: Nanos,
    /// The predicted display time.
    display: Nanos,
    /// The vsync the frame starts at, see [start_vsyncs].
    start: Nanos,
    /// Vsyncs the previous frame was shown again for before this one.
    missed_before: u32,
    /// Vsyncs this frame was shown again for, known once the next frame is waited for.
    repeats: u32,
    /// Time spent in Submit.
    submitting: Nanos,
    /// The last Submit returned.
    frame_ready: Option<Nanos>,
    gpu_marked: bool,
    /// The GPU finished the work submitted up to the last Submit.
    gpu_done: Option<Nanos>,
    /// Handing the frame to the runtime started.
    update_start: Option<Nanos>,
    /// xrEndFrame was called and returned.
    end_call: Option<(Nanos, Nanos)>,
    /// Handing the frame to the runtime ended.
    ended: Nanos,
    /// The frame showed the app's images, not just overlays.
    app_content: bool,
    pose: Option<vr::TrackedDevicePose_t>,
}

#[derive(Default)]
struct State {
    next_seq: u64,
    /// The WaitGetPoses call in progress.
    wait_get_poses_called: Option<Nanos>,
    /// When the last waited frame was asked for.
    last_called: Option<Nanos>,
    /// See [start_vsyncs].
    start_vsyncs: Option<i64>,
    /// The runtime's time minus ours, from reading both one after the other. The smallest of the
    /// last few is the least delayed by the thread being preempted in between.
    clock_offsets: VecDeque<Nanos>,
    waited: Option<Frame>,
    begun: Option<Frame>,
    /// Ended frames, oldest first.
    history: VecDeque<Frame>,
    /// What the frames that fell out of `history` add up to.
    evicted: Totals,
}

/// What the GPU times of the frames before the next one were.
#[derive(Clone, Copy, Default)]
struct GpuCarry {
    /// When the GPU finished the last frame it reported.
    last_done: Option<Nanos>,
    /// That frame's GPU time.
    last_time: Option<Nanos>,
}

#[derive(Clone, Copy, Default)]
struct Totals {
    frames: u32,
    repeats: u32,
    submits: u32,
    target_ms: f64,
    compositor_cpu_ms: f64,
    app_cpu_ms: f64,
    app_gpu_ms: f64,
    carry: GpuCarry,
}

fn nanos(duration: Duration) -> Nanos {
    duration.as_nanos().try_into().unwrap_or(Nanos::MAX)
}

fn ms(nanos: Nanos) -> f32 {
    nanos as f32 / 1e6
}

/// Whole periods from the vsync a frame starts at to its predicted display time, `lead` after the
/// runtime released the frame. SteamVR's frame starts at the vsync after running start, which
/// xrWaitFrame returning stands in for. The count is kept while that vsync stays between a quarter
/// period before the release and a period and a quarter after it, so frame times don't jump
/// between vsyncs when the release jitters around one.
fn start_vsyncs(previous: Option<i64>, lead: Nanos, period: Nanos) -> i64 {
    if period <= 0 {
        return 0;
    }
    let periods = lead as f64 / period as f64;
    match previous {
        Some(vsyncs) if (vsyncs as f64 - 0.25..vsyncs as f64 + 1.25).contains(&periods) => vsyncs,
        _ => periods.floor().max(0.0) as i64,
    }
}

/// The GPU time of `frame`: from when it started, or the GPU finished the frame before it if that
/// was later, to when the GPU finished it. Like SteamVR's timestamps, this includes the GPU
/// waiting for the app to submit work. Frames the backend didn't report get the last reported
/// time, or an estimate before any, which isn't measured.
fn gpu_time(frame: &Frame, carry: &mut GpuCarry) -> (Nanos, bool) {
    match frame.gpu_done {
        Some(done) => {
            let start = frame
                .render_start()
                .max(carry.last_done.unwrap_or(Nanos::MIN));
            let time = (done - start).max(0);
            *carry = GpuCarry {
                last_done: Some(done),
                last_time: Some(time),
            };
            (time, true)
        }
        None => carry
            .last_time
            .map_or_else(|| (frame.estimated_gpu_time(), false), |time| (time, true)),
    }
}

impl Frame {
    /// When the app could start rendering the frame.
    fn render_start(&self) -> Nanos {
        self.poses_ready.unwrap_or(self.wait_end)
    }

    /// From the frame's poses to its last Submit.
    fn app_cpu_time(&self) -> Nanos {
        self.frame_ready
            .map_or(0, |ready| (ready - self.render_start()).max(0))
    }

    /// Without a measurement, the GPU took at least as long as the app took to submit the frame,
    /// and a period if the next frame came late.
    fn estimated_gpu_time(&self) -> Nanos {
        let submitting = self.app_cpu_time();
        if self.repeats > 0 {
            submitting.max(self.period)
        } else {
            submitting
        }
    }

    fn vr_timing(&self, (gpu, measured): (Nanos, bool)) -> vr::Compositor_FrameTiming {
        let since_start = |time: Nanos| ms(time - self.start);
        let update_start = self.update_start.unwrap_or(self.ended);
        let update_end = self.end_call.map_or(self.ended, |(_, end)| end);
        let mut flags = vr::VRCompositor_ReprojectionAsync;
        if self.repeats > 0 {
            // Unless the GPU took longer than a period, the app was late on the CPU.
            flags |= if measured && gpu > self.period {
                vr::VRCompositor_ReprojectionReason_Gpu
            } else {
                vr::VRCompositor_ReprojectionReason_Cpu
            };
        }
        vr::Compositor_FrameTiming {
            m_nSize: size_of::<vr::Compositor_FrameTiming>() as u32,
            m_nFrameIndex: self.index,
            m_nNumFramePresents: self.repeats.saturating_add(1),
            // Only frames finished after their display time are sure to have missed it.
            m_nNumMisPresented: u32::from(self.gpu_done.unwrap_or(update_end) > self.display),
            m_nNumDroppedFrames: self.missed_before,
            m_nReprojectionFlags: flags,
            m_flSystemTimeInSeconds: self.start as f64 / 1e9,
            m_flPreSubmitGpuMs: ms(gpu),
            // Work after the last Submit is part of the next frame's.
            m_flPostSubmitGpuMs: 0.0,
            m_flTotalRenderGpuMs: ms(gpu),
            // The runtime's composition happens in its own process.
            m_flCompositorRenderGpuMs: 0.0,
            m_flCompositorRenderCpuMs: ms(self.ended - update_start),
            m_flCompositorIdleCpuMs: ms(self.wait_end - self.wait_start),
            m_flClientFrameIntervalMs: ms(self.interval.unwrap_or(self.period)),
            m_flPresentCallCpuMs: self.end_call.map_or(0.0, |(start, end)| ms(end - start)),
            m_flWaitForPresentCpuMs: 0.0,
            m_flSubmitFrameMs: ms(self.submitting),
            m_flWaitGetPosesCalledMs: since_start(self.called),
            m_flNewPosesReadyMs: since_start(self.render_start()),
            m_flNewFrameReadyMs: since_start(self.frame_ready.unwrap_or(update_start)),
            m_flCompositorUpdateStartMs: since_start(update_start),
            m_flCompositorUpdateEndMs: since_start(update_end),
            // The runtime can start composing once it has the frame.
            m_flCompositorRenderStartMs: since_start(update_end),
            m_HmdPose: self.pose.unwrap_or_default(),
            ..Default::default()
        }
    }
}

impl Totals {
    fn add(&mut self, frame: &Frame) {
        let (gpu, _) = gpu_time(frame, &mut self.carry);
        self.frames = self.frames.saturating_add(1);
        self.repeats = self.repeats.saturating_add(frame.repeats);
        if frame.app_content {
            let ms = |nanos: Nanos| nanos as f64 / 1e6;
            self.submits = self.submits.saturating_add(1);
            self.target_ms += ms(frame.period);
            self.compositor_cpu_ms += ms(frame.ended - frame.update_start.unwrap_or(frame.ended));
            self.app_cpu_ms += ms(frame.app_cpu_time());
            self.app_gpu_ms += ms(gpu);
        }
    }
}

impl State {
    fn frame_mut(&mut self, seq: u64) -> Option<&mut Frame> {
        if self.waited.as_ref().is_some_and(|frame| frame.seq == seq) {
            return self.waited.as_mut();
        }
        if self.begun.as_ref().is_some_and(|frame| frame.seq == seq) {
            return self.begun.as_mut();
        }
        self.history.iter_mut().rev().find(|frame| frame.seq == seq)
    }
}

impl FrameTimer {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            state: Mutex::default(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn nanos(&self, time: Instant) -> Nanos {
        nanos(time.saturating_duration_since(self.origin))
    }

    pub fn wait_get_poses_called(&self, at: Instant) {
        let at = self.nanos(at);
        self.state().wait_get_poses_called = Some(at);
    }

    pub fn frame_waited(&self, frame: WaitedFrame) {
        let wait_start = self.nanos(frame.wait_start);
        let wait_end = self.nanos(frame.wait_end);
        let period = frame.period.max(0);

        let mut state = self.state();
        if let Some((ours, theirs)) = frame.now {
            if state.clock_offsets.len() == CLOCK_SAMPLES {
                state.clock_offsets.pop_front();
            }
            let offset = theirs.as_nanos() - self.nanos(ours);
            state.clock_offsets.push_back(offset);
        }
        // Without the runtime's clock, the frame is assumed to be shown a period after its release.
        let lead = state
            .clock_offsets
            .iter()
            .min()
            .map(|offset| frame.display_time.as_nanos() - offset - wait_end)
            .filter(|lead| lead.abs() <= MAX_LEAD)
            .unwrap_or(period);
        let display = wait_end + lead;
        let seq = state.next_seq;
        state.next_seq += 1;
        let called = state
            .wait_get_poses_called
            .map_or(wait_start, |called| called.min(wait_start));
        let interval = state.last_called.replace(called).map(|last| called - last);
        let vsyncs = start_vsyncs(state.start_vsyncs, lead, period);
        state.start_vsyncs = Some(vsyncs);
        if let Some(previous) = seq.checked_sub(1).and_then(|seq| state.frame_mut(seq)) {
            previous.repeats = frame.missed;
        }
        state.waited = Some(Frame {
            seq,
            called,
            interval,
            wait_start,
            wait_end,
            period,
            display,
            start: display - vsyncs * period,
            missed_before: frame.missed,
            ..Default::default()
        });
    }

    /// WaitGetPoses returned the newest frame's poses, with the HMD's if the app asked for them.
    pub fn wait_get_poses_returned(&self, at: Instant, pose: Option<vr::TrackedDevicePose_t>) {
        let at = self.nanos(at);
        let mut state = self.state();
        state.wait_get_poses_called = None;
        let State { waited, begun, .. } = &mut *state;
        if let Some(frame) = waited
            .as_mut()
            .or(begun.as_mut())
            .filter(|frame| frame.poses_ready.is_none())
        {
            frame.poses_ready = Some(at);
            frame.pose = pose;
        }
    }

    /// The waited frame was begun. A begun frame that wasn't ended is discarded.
    pub fn frame_begun(&self) {
        let mut state = self.state();
        state.begun = state.waited.take();
    }

    /// Where to report when the GPU finished the begun frame's work, once per frame.
    pub fn gpu_mark(self: &Arc<Self>) -> Option<GpuDone> {
        let mut state = self.state();
        let frame = state.begun.as_mut().filter(|frame| !frame.gpu_marked)?;
        frame.gpu_marked = true;
        let (timer, seq) = (self.clone(), frame.seq);
        Some(GpuDone::new(move |done| timer.gpu_done(seq, done)))
    }

    fn gpu_done(&self, seq: u64, done: Instant) {
        let done = self.nanos(done);
        if let Some(frame) = self.state().frame_mut(seq) {
            frame.gpu_done = Some(done);
        }
    }

    /// A Submit call on the begun frame took from `start` to `end`.
    pub fn submitted(&self, start: Instant, end: Instant) {
        let (start, end) = (self.nanos(start), self.nanos(end));
        if let Some(frame) = self.state().begun.as_mut() {
            frame.submitting += end - start;
            frame.frame_ready = Some(end);
        }
    }

    /// The begun frame's xrEndFrame call took from `start` to `end`.
    pub fn end_frame_called(&self, start: Instant, end: Instant) {
        let (start, end) = (self.nanos(start), self.nanos(end));
        if let Some(frame) = self.state().begun.as_mut() {
            frame.end_call = Some((start, end));
        }
    }

    /// Handing the begun frame to the runtime took from `start` to `end`. `app_content` is whether
    /// it showed the app's images.
    pub fn frame_ended(&self, index: u32, start: Instant, end: Instant, app_content: bool) {
        let (start, end) = (self.nanos(start), self.nanos(end));
        let mut state = self.state();
        let Some(mut frame) = state.begun.take() else {
            return;
        };
        frame.index = index;
        frame.update_start = Some(start);
        frame.ended = end;
        frame.app_content = app_content;
        state.history.push_back(frame);
        if state.history.len() > HISTORY
            && let Some(oldest) = state.history.pop_front()
        {
            state.evicted.add(&oldest);
        }
    }

    /// The timing of the frame ended `frames_ago` frames before the last one, or of the oldest one
    /// kept, and whether it has the pose the app rendered it with. None before any frame ended.
    pub fn frame_timing(&self, frames_ago: u32) -> Option<(vr::Compositor_FrameTiming, bool)> {
        let state = self.state();
        let last = state.history.len().checked_sub(1)?;
        let index = last.saturating_sub(frames_ago as usize);
        let mut carry = state.evicted.carry;
        for frame in state.history.range(..index) {
            gpu_time(frame, &mut carry);
        }
        let frame = &state.history[index];
        Some((
            frame.vr_timing(gpu_time(frame, &mut carry)),
            frame.pose.is_some(),
        ))
    }

    /// The number of ended frames kept.
    pub fn frame_count(&self) -> usize {
        self.state().history.len()
    }

    /// For GetFrameTiming before any frame ended.
    pub fn no_frame_timing(&self, index: u32, period: Duration) -> vr::Compositor_FrameTiming {
        vr::Compositor_FrameTiming {
            m_nSize: size_of::<vr::Compositor_FrameTiming>() as u32,
            m_nFrameIndex: index,
            m_nNumFramePresents: 1,
            m_nReprojectionFlags: vr::VRCompositor_ReprojectionAsync,
            m_flSystemTimeInSeconds: self.origin.elapsed().as_secs_f64(),
            m_flClientFrameIntervalMs: ms(nanos(period)),
            ..Default::default()
        }
    }

    pub fn cumulative_stats(&self) -> vr::Compositor_CumulativeStats {
        let state = self.state();
        let mut totals = state.evicted;
        for frame in &state.history {
            totals.add(frame);
        }
        vr::Compositor_CumulativeStats {
            m_nPid: std::process::id(),
            m_nNumFramePresents: totals.frames.saturating_add(totals.repeats),
            // The runtime reprojects the frames it shows again. Its own misses are unknown.
            m_nNumDroppedFrames: 0,
            m_nNumReprojectedFrames: totals.repeats,
            m_nNumFrameSubmits: totals.submits,
            m_flSumCompositorCPUTimeMS: vr::vrshared_double(totals.compositor_cpu_ms),
            m_flSumTargetFrameTimes: vr::vrshared_double(totals.target_ms),
            m_flSumApplicationCPUTimeMS: vr::vrshared_double(totals.app_cpu_ms),
            m_flSumApplicationGPUTimeMS: vr::vrshared_double(totals.app_gpu_ms),
            ..Default::default()
        }
    }

    /// The time left until the runtime is expected to release the next frame, or None if the app
    /// isn't in a frame loop.
    pub fn time_remaining(&self, now: Instant) -> Option<Duration> {
        let now = self.nanos(now);
        let state = self.state();
        let frame = state
            .waited
            .as_ref()
            .or(state.begun.as_ref())
            .or(state.history.back())?;
        if now - frame.wait_end > STALE {
            return None;
        }
        let left = frame.wait_end + frame.period - now;
        Some(Duration::from_nanos(left.max(0) as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(timer: &FrameTimer, ms: f64) -> Instant {
        timer.origin + Duration::from_micros((ms * 1000.0).round() as u64)
    }

    const PERIOD: i64 = 10_000_000;

    /// Runs a frame waited for at `start` ms, displayed `lead` ms after that, with its last Submit
    /// `submit` ms after the poses and the GPU done `gpu` ms after them.
    fn frame(timer: &Arc<FrameTimer>, index: u32, start: f64, missed: u32, gpu: Option<f64>) {
        let lead = 15.0;
        timer.wait_get_poses_called(at(timer, start - 2.0));
        timer.frame_waited(WaitedFrame {
            display_time: xr::Time::from_nanos(((start + lead) * 1e6) as i64),
            period: PERIOD,
            missed,
            wait_start: at(timer, start - 1.0),
            wait_end: at(timer, start),
            now: Some((at(timer, start), xr::Time::from_nanos((start * 1e6) as i64))),
        });
        timer.frame_begun();
        timer.wait_get_poses_returned(at(timer, start + 0.1), None);
        timer.submitted(at(timer, start + 3.0), at(timer, start + 3.5));
        let mark = timer.gpu_mark();
        if let (Some(mark), Some(gpu)) = (mark, gpu) {
            mark.report(at(timer, start + 0.1 + gpu));
        }
        timer.end_frame_called(at(timer, start + 8.1), at(timer, start + 8.4));
        timer.frame_ended(index, at(timer, start + 8.0), at(timer, start + 8.5), true);
    }

    fn timing(timer: &FrameTimer, frames_ago: u32) -> vr::Compositor_FrameTiming {
        timer.frame_timing(frames_ago).unwrap().0
    }

    #[track_caller]
    fn assert_near(value: f32, expected: f32) {
        assert!((value - expected).abs() < 0.01, "{value} != {expected}");
    }

    #[test]
    fn start_vsync_after_release() {
        assert_eq!(start_vsyncs(None, 15, 10), 1);
        assert_eq!(start_vsyncs(None, 5, 10), 0);
        assert_eq!(start_vsyncs(None, 30, 10), 3);
        // Kept while the release jitters around a vsync.
        assert_eq!(start_vsyncs(Some(1), 21, 10), 1);
        assert_eq!(start_vsyncs(Some(2), 19, 10), 2);
        // A real change in latency moves it.
        assert_eq!(start_vsyncs(Some(1), 26, 10), 2);
        assert_eq!(start_vsyncs(Some(3), 15, 10), 1);
        assert_eq!(start_vsyncs(Some(3), 15, 0), 0);
    }

    #[test]
    fn phases() {
        let timer = Arc::new(FrameTimer::new());
        assert!(timer.frame_timing(0).is_none());
        frame(&timer, 0, 100.0, 0, Some(5.0));
        frame(&timer, 1, 110.0, 0, Some(5.0));
        let t = timing(&timer, 0);
        assert_eq!({ t.m_nFrameIndex }, 1);
        assert_eq!({ t.m_nNumFramePresents }, 1);
        assert_eq!({ t.m_nNumDroppedFrames }, 0);
        assert_eq!({ t.m_nNumMisPresented }, 0);
        assert_eq!(
            { t.m_nReprojectionFlags },
            vr::VRCompositor_ReprojectionAsync
        );
        // Shown 15 ms after its release, so it starts at the vsync 5 ms after it.
        assert!(({ t.m_flSystemTimeInSeconds } - 0.115).abs() < 1e-6);
        assert_near(t.m_flWaitGetPosesCalledMs, -7.0);
        assert_near(t.m_flNewPosesReadyMs, -4.9);
        assert_near(t.m_flNewFrameReadyMs, -1.5);
        assert_near(t.m_flCompositorUpdateStartMs, 3.0);
        assert_near(t.m_flCompositorUpdateEndMs, 3.4);
        assert_near(t.m_flCompositorRenderStartMs, 3.4);
        assert_near(t.m_flClientFrameIntervalMs, 10.0);
        assert_near(t.m_flCompositorIdleCpuMs, 1.0);
        assert_near(t.m_flSubmitFrameMs, 0.5);
        assert_near(t.m_flPresentCallCpuMs, 0.3);
        assert_near(t.m_flCompositorRenderCpuMs, 0.5);
        assert_near(t.m_flPreSubmitGpuMs, 5.0);
        assert_near(t.m_flTotalRenderGpuMs, 5.0);
        // The oldest frame is the one past the history too.
        assert_eq!({ timing(&timer, 1).m_nFrameIndex }, 0);
        assert_eq!({ timing(&timer, 5).m_nFrameIndex }, 0);
    }

    #[test]
    fn gpu_bound_frames_start_when_the_last_one_finished() {
        let timer = Arc::new(FrameTimer::new());
        frame(&timer, 0, 100.0, 0, Some(14.0));
        frame(&timer, 1, 110.0, 0, Some(14.0));
        // Frame 1's work waited for frame 0's to finish at 114.1.
        assert_near(timing(&timer, 0).m_flTotalRenderGpuMs, 10.0);
        assert_near(timing(&timer, 1).m_flTotalRenderGpuMs, 14.0);
    }

    #[test]
    fn unreported_gpu_time() {
        let timer = Arc::new(FrameTimer::new());
        // Without any report, the GPU took at least as long as submitting.
        frame(&timer, 0, 100.0, 0, None);
        assert_near(timing(&timer, 0).m_flTotalRenderGpuMs, 3.4);
        // Not reported yet: the last reported time.
        frame(&timer, 1, 110.0, 0, Some(6.0));
        frame(&timer, 2, 120.0, 0, None);
        assert_near(timing(&timer, 0).m_flTotalRenderGpuMs, 6.0);
        // A report that arrives later replaces it.
        let seq = timer.state().history.back().unwrap().seq;
        timer.gpu_done(seq, at(&timer, 120.1 + 7.0));
        assert_near(timing(&timer, 0).m_flTotalRenderGpuMs, 7.0);
    }

    #[test]
    fn missed_vsyncs_reproject_the_last_frame() {
        let timer = Arc::new(FrameTimer::new());
        frame(&timer, 0, 100.0, 0, Some(5.0));
        // Frame 1 is shown 3 vsyncs after frame 0.
        frame(&timer, 1, 130.0, 2, Some(5.0));
        frame(&timer, 2, 140.0, 0, Some(5.0));
        let late = timing(&timer, 2);
        assert_eq!({ late.m_nNumFramePresents }, 3);
        assert_eq!(
            { late.m_nReprojectionFlags },
            vr::VRCompositor_ReprojectionAsync | vr::VRCompositor_ReprojectionReason_Cpu
        );
        let next = timing(&timer, 1);
        assert_eq!({ next.m_nNumDroppedFrames }, 2);
        assert_eq!({ next.m_nNumFramePresents }, 1);
        assert_near(next.m_flClientFrameIntervalMs, 30.0);

        let stats = timer.cumulative_stats();
        assert_eq!(stats.m_nNumFramePresents, 5);
        assert_eq!(stats.m_nNumReprojectedFrames, 2);
        assert_eq!(stats.m_nNumDroppedFrames, 0);
        assert_eq!(stats.m_nNumFrameSubmits, 3);
        assert!((stats.m_flSumTargetFrameTimes.0 - 30.0).abs() < 1e-6);
        assert!((stats.m_flSumApplicationGPUTimeMS.0 - 15.0).abs() < 1e-3);
        assert!((stats.m_flSumApplicationCPUTimeMS.0 - 3.4 * 3.0).abs() < 1e-3);
    }

    #[test]
    fn slow_gpu_is_the_reprojection_reason() {
        let timer = Arc::new(FrameTimer::new());
        frame(&timer, 0, 100.0, 0, Some(12.0));
        frame(&timer, 1, 120.0, 1, Some(5.0));
        assert_eq!(
            { timing(&timer, 1).m_nReprojectionFlags },
            vr::VRCompositor_ReprojectionAsync | vr::VRCompositor_ReprojectionReason_Gpu
        );
    }

    #[test]
    fn frames_done_after_their_display_time_are_mispresented() {
        let timer = Arc::new(FrameTimer::new());
        frame(&timer, 0, 100.0, 0, Some(16.0));
        assert_eq!({ timing(&timer, 0).m_nNumMisPresented }, 1);
    }

    #[test]
    fn totals_outlive_the_history() {
        let timer = Arc::new(FrameTimer::new());
        let frames = HISTORY as u32 + 10;
        for index in 0..frames {
            frame(&timer, index, 100.0 + 10.0 * index as f64, 0, Some(5.0));
        }
        assert_eq!(timer.frame_count(), HISTORY);
        let stats = timer.cumulative_stats();
        assert_eq!(stats.m_nNumFramePresents, frames);
        assert_eq!(stats.m_nNumFrameSubmits, frames);
        assert!((stats.m_flSumApplicationGPUTimeMS.0 - 5.0 * frames as f64).abs() < 1e-2);
        assert_eq!({ timing(&timer, HISTORY as u32).m_nFrameIndex }, 10);
    }

    #[test]
    fn time_remaining_in_the_frame() {
        let timer = Arc::new(FrameTimer::new());
        assert_eq!(timer.time_remaining(at(&timer, 0.0)), None);
        frame(&timer, 0, 100.0, 0, None);
        let left = |ms| timer.time_remaining(at(&timer, ms));
        assert_eq!(left(104.0), Some(Duration::from_millis(6)));
        assert_eq!(left(115.0), Some(Duration::ZERO));
        assert_eq!(left(400.0), None);
    }

    #[test]
    fn display_time_without_runtime_clock() {
        let timer = Arc::new(FrameTimer::new());
        timer.frame_waited(WaitedFrame {
            display_time: xr::Time::from_nanos(1),
            period: PERIOD,
            missed: 0,
            wait_start: at(&timer, 99.0),
            wait_end: at(&timer, 100.0),
            now: None,
        });
        timer.frame_begun();
        timer.frame_ended(0, at(&timer, 101.0), at(&timer, 102.0), true);
        let t = timing(&timer, 0);
        // Shown a period after its release, which is the vsync it starts at.
        assert!(({ t.m_flSystemTimeInSeconds } - 0.1).abs() < 1e-6);
        assert_near(t.m_flWaitGetPosesCalledMs, -1.0);
        assert_eq!({ t.m_nNumMisPresented }, 0);
    }
}
