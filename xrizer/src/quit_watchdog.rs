//! Kills an app that keeps running after the runtime asked it to quit, like SteamVR does.

use log::{debug, error, info, warn};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How long SteamVR gives an app to quit after VREvent_Quit before killing it, going by the
/// "Kill process ... because it didn't quit in time" lines in its logs.
const TIMEOUT: Duration = Duration::from_secs(5);
/// How long an app gets after AcknowledgeQuit_Exiting. SteamVR extends its timeout then, by an
/// amount it doesn't document.
const ACKNOWLEDGED_TIMEOUT: Duration = Duration::from_secs(30);

type Kill = Arc<dyn Fn() + Send + Sync>;

pub struct QuitWatchdog {
    state: Arc<(Mutex<State>, Condvar)>,
    timeout: Duration,
    acknowledged_timeout: Duration,
    kill: Kill,
}

#[derive(Default)]
struct State {
    deadline: Option<Instant>,
    acknowledged: bool,
    /// Set once the app shut down or is prompting the user, so it's never killed.
    stopped: bool,
}

impl Default for QuitWatchdog {
    fn default() -> Self {
        Self::with(TIMEOUT, ACKNOWLEDGED_TIMEOUT, Arc::new(kill_process))
    }
}

#[cfg(not(test))]
fn kill_process() {
    // Plain exit would run the app's exit handlers, which may wait on the threads that hang.
    unsafe { libc::_exit(1) }
}

#[cfg(test)]
fn kill_process() {}

impl QuitWatchdog {
    fn with(timeout: Duration, acknowledged_timeout: Duration, kill: Kill) -> Self {
        Self {
            state: Default::default(),
            timeout,
            acknowledged_timeout,
            kill,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts the timeout once the app was told to quit.
    pub fn start(&self) {
        if crate::quirks::env_override("XRIZER_KILL_ON_QUIT") == Some(false) {
            info!("Not killing the application if it doesn't quit (XRIZER_KILL_ON_QUIT=0)");
            return;
        }
        let mut state = self.lock();
        if state.stopped || state.deadline.is_some() {
            return;
        }
        state.deadline = Some(Instant::now() + self.timeout);
        drop(state);

        let shared = self.state.clone();
        let kill = self.kill.clone();
        let spawned = std::thread::Builder::new()
            .name("xrizer quit watchdog".into())
            .spawn(move || watch(&shared, &*kill));
        if let Err(e) = spawned {
            warn!("Failed to start the quit watchdog: {e}");
        }
    }

    /// The app is exiting (AcknowledgeQuit_Exiting), which gives it longer, once.
    pub fn acknowledged(&self) {
        let mut state = self.lock();
        let Some(deadline) = state.deadline else {
            return;
        };
        if !std::mem::replace(&mut state.acknowledged, true) {
            info!("The application acknowledged the quit");
            state.deadline = Some(deadline.max(Instant::now() + self.acknowledged_timeout));
            self.state.1.notify_all();
        }
    }

    /// The app shut down (VR_Shutdown) or prompts the user (AcknowledgeQuit_UserPrompt), so it's
    /// left alone from now on.
    pub fn stop(&self) {
        let mut state = self.lock();
        state.stopped = true;
        if state.deadline.take().is_some() {
            debug!("quit watchdog stopped");
            self.state.1.notify_all();
        }
    }

    #[cfg(test)]
    pub fn deadline(&self) -> Option<Instant> {
        self.lock().deadline
    }
}

impl Drop for QuitWatchdog {
    fn drop(&mut self) {
        self.stop();
    }
}

fn watch((state, changed): &(Mutex<State>, Condvar), kill: &(dyn Fn() + Send + Sync)) {
    let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        let Some(deadline) = guard.deadline else {
            return;
        };
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        guard = changed
            .wait_timeout(guard, deadline - now)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
    guard.deadline = None;
    guard.stopped = true;
    drop(guard);
    error!("The application didn't quit in time after the runtime asked it to, killing it");
    kill();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn watchdog(
        timeout: Duration,
        acknowledged_timeout: Duration,
    ) -> (QuitWatchdog, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let kill: Kill = Arc::new(move || {
            let _ = tx.lock().unwrap().send(());
        });
        (QuitWatchdog::with(timeout, acknowledged_timeout, kill), rx)
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn kills_after_timeout() {
        let (watchdog, killed) = watchdog(Duration::from_millis(10), Duration::from_secs(60));
        let start = Instant::now();
        watchdog.start();
        killed.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(10));
        // Only once.
        watchdog.start();
        assert!(killed.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn acknowledging_extends_timeout() {
        let (watchdog, killed) = watchdog(Duration::from_secs(60), Duration::from_secs(600));
        watchdog.acknowledged();
        assert_eq!(watchdog.deadline(), None, "acknowledged before any quit");

        let before = Instant::now();
        watchdog.start();
        let first = watchdog.deadline().unwrap();
        assert!(first >= before + Duration::from_secs(60));
        watchdog.acknowledged();
        let extended = watchdog.deadline().unwrap();
        assert!(extended >= first + Duration::from_secs(500));
        // Only once, so an app acknowledging every frame still gets killed.
        watchdog.acknowledged();
        assert_eq!(watchdog.deadline(), Some(extended));
        drop(watchdog);
        assert!(killed.recv().is_err(), "killed after drop");
    }

    /// Stopping is for good: a quit after the app shut down never kills it.
    #[test]
    fn stop_disarms() {
        let (watchdog, killed) = watchdog(Duration::from_secs(60), Duration::from_secs(60));
        watchdog.start();
        watchdog.stop();
        assert_eq!(watchdog.deadline(), None);
        watchdog.start();
        assert_eq!(watchdog.deadline(), None);
        drop(watchdog);
        assert!(killed.recv().is_err());
    }
}
