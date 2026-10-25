//! When the GPU finished the work an app submitted for each frame, from GL_TIMESTAMP queries
//! written after it. They are read once available, a frame or more later, and moved to the CPU's
//! clock with the GPU's time read when they were written.

use crate::compositor::GpuDone;
use gl::types::{GLint, GLint64, GLuint, GLuint64};
use std::collections::VecDeque;
use std::ffi::CStr;
use std::time::{Duration, Instant};

/// With this many frames pending, the GPU is far behind and the next ones go unmeasured.
const MAX_QUERIES: usize = 4;

/// Timestamps further than this from the GPU's time when they were written are garbage, e.g. from
/// a counter that wrapped around.
const MAX_SKEW: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct GpuTimer {
    /// Whether the context has timestamp queries, once checked.
    supported: Option<bool>,
    free: Vec<GLuint>,
    pending: VecDeque<Pending>,
}

struct Pending {
    query: GLuint,
    /// The GPU's clock when the query was written, and the CPU's.
    gpu_now: GLint64,
    cpu_now: Instant,
    done: GpuDone,
}

impl GpuTimer {
    /// Writes a timestamp after the work submitted so far, which is reported to `done` once a
    /// later frame finds it available. The context it was set up with must be current.
    pub fn frame_submitted(&mut self, done: GpuDone) {
        if !*self.supported.get_or_insert_with(|| unsafe { supported() }) {
            return;
        }
        self.poll();
        let query = match self.free.pop() {
            Some(query) => query,
            None if self.pending.len() < MAX_QUERIES => {
                let mut query = 0;
                unsafe { gl::GenQueries(1, &mut query) };
                query
            }
            None => return,
        };
        let mut gpu_now = 0;
        unsafe {
            gl::QueryCounter(query, gl::TIMESTAMP);
            // Otherwise drivers like Mesa's keep the frame's work until the app's next flush,
            // which the timestamp would wait for.
            gl::Flush();
            // The GPU's clock now, while the work before the query may still be running.
            gl::GetInteger64v(gl::TIMESTAMP, &mut gpu_now);
        }
        self.pending.push_back(Pending {
            query,
            gpu_now,
            cpu_now: Instant::now(),
            done,
        });
    }

    /// Reports the frames whose timestamps are available, oldest first, without waiting.
    fn poll(&mut self) {
        while let Some(pending) = self.pending.front() {
            let mut available: GLint = 0;
            unsafe {
                gl::GetQueryObjectiv(pending.query, gl::QUERY_RESULT_AVAILABLE, &mut available)
            };
            if available == 0 {
                break;
            }
            let mut timestamp: GLuint64 = 0;
            unsafe { gl::GetQueryObjectui64v(pending.query, gl::QUERY_RESULT, &mut timestamp) };
            let Some(pending) = self.pending.pop_front() else {
                break;
            };
            self.free.push(pending.query);
            if let Some(done) = gpu_to_cpu_time(timestamp, pending.gpu_now, pending.cpu_now) {
                pending.done.report(done);
            }
        }
    }

    /// Deletes the queries. The context they were created in must be current.
    pub fn destroy(&mut self) {
        let queries: Vec<GLuint> = self
            .free
            .drain(..)
            .chain(self.pending.drain(..).map(|pending| pending.query))
            .collect();
        if !queries.is_empty() {
            unsafe { gl::DeleteQueries(queries.len() as i32, queries.as_ptr()) };
        }
    }
}

/// Whether the current context has timestamp queries: OpenGL 3.3 or GL_ARB_timer_query.
unsafe fn supported() -> bool {
    let loaded = gl::GenQueries::is_loaded()
        && gl::DeleteQueries::is_loaded()
        && gl::QueryCounter::is_loaded()
        && gl::GetQueryiv::is_loaded()
        && gl::GetQueryObjectiv::is_loaded()
        && gl::GetQueryObjectui64v::is_loaded()
        && gl::GetInteger64v::is_loaded();
    if !loaded {
        return false;
    }
    let version = unsafe { gl::GetString(gl::VERSION) };
    let version = (!version.is_null())
        .then(|| unsafe { CStr::from_ptr(version.cast()) })
        .and_then(|version| gl_version(version.to_bytes()));
    if version.is_none_or(|version| version < (3, 3))
        && !super::has_gl_extension(c"GL_ARB_timer_query")
    {
        return false;
    }
    let mut bits = 0;
    unsafe { gl::GetQueryiv(gl::TIMESTAMP, gl::QUERY_COUNTER_BITS, &mut bits) };
    bits > 0
}

/// The major and minor version at the start of a GL_VERSION string, e.g. "4.6.0 NVIDIA 580.95".
fn gl_version(version: &[u8]) -> Option<(u32, u32)> {
    let version = std::str::from_utf8(version).ok()?;
    let mut numbers = version
        .split(|c: char| !c.is_ascii_digit())
        .map(str::parse::<u32>);
    Some((numbers.next()?.ok()?, numbers.next()?.ok()?))
}

/// When on the CPU's clock the GPU's clock read `timestamp`, given that it read `gpu_now` at
/// `cpu_now`.
fn gpu_to_cpu_time(timestamp: GLuint64, gpu_now: GLint64, cpu_now: Instant) -> Option<Instant> {
    let delta = i128::from(timestamp) - i128::from(gpu_now);
    let skew = Duration::from_nanos(u64::try_from(delta.unsigned_abs()).ok()?);
    if skew > MAX_SKEW {
        return None;
    }
    if delta >= 0 {
        Some(cpu_now + skew)
    } else {
        cpu_now.checked_sub(skew)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(gl_version(b"4.6.0 NVIDIA 580.95.05"), Some((4, 6)));
        assert_eq!(gl_version(b"3.3 (Core Profile) Mesa 25.2.4"), Some((3, 3)));
        assert_eq!(gl_version(b"2.1 Mesa"), Some((2, 1)));
        assert_eq!(gl_version(b"garbage"), None);
        assert_eq!(gl_version(b""), None);
    }

    #[test]
    fn timestamps_on_the_cpu_clock() {
        let now = Instant::now() + Duration::from_secs(10);
        let ms = Duration::from_millis;
        assert_eq!(
            gpu_to_cpu_time(5_000_000, 2_000_000, now),
            Some(now + ms(3))
        );
        // Done before the GPU's clock was read.
        assert_eq!(
            gpu_to_cpu_time(1_000_000, 2_000_000, now),
            Some(now - ms(1))
        );
        assert_eq!(gpu_to_cpu_time(5_000_000_000, 2_000_000, now), None);
        assert_eq!(gpu_to_cpu_time(u64::MAX, i64::MIN, now), None);
    }
}
