//! Apps the Library started, kept for the whole dashboard as a page is built again on each visit.
//! Monado knows a session by the name its app gives, for OpenVR games the executable's such as
//! hlvr, so a session the Library started is shown with the Library's title. WayVR looks for that
//! session on every frame through [`poll`], as the dashboard doesn't update while it is hidden.

use std::{
	cell::RefCell,
	collections::HashMap,
	time::{Duration, Instant},
};

use wlx_common::dash_interface::MonadoClient;

/// Steam can take a while to update or start a game, after this the start is given up
const STARTING_TIMEOUT: Duration = Duration::from_secs(180);
/// How often [`poll`] lists the sessions while a start is pending
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// An app that was started and has no session yet
struct Start {
	id: String,
	/// None when the next session may be another app's
	title: Option<String>,
	/// Clients that were connected before the start
	known: Vec<i64>,
	/// The LvrrState generation at the start, a failure that bumps it later is this start's
	generation: u64,
	since: Instant,
}

#[derive(Default)]
struct Started {
	start: Option<Start>,
	/// When [`poll`] last listed the sessions
	polled: Option<Instant>,
	/// By Monado client id. The server never reuses one, and the VR dashboard, the one with the
	/// Library, stops with it.
	titles: HashMap<i64, String>,
}

thread_local! {
	static STARTED: RefCell<Started> = RefCell::default();
}

/// Notes that the Library started app `id`. `known` are the clients connected before, None when
/// Monado couldn't list them. `generation` is the LvrrState's, see [`note_failed`].
pub fn note_start(id: String, name: &str, known: Option<Vec<i64>>, generation: u64) {
	STARTED.with_borrow_mut(|started| started.note_start(id, name, known, generation, Instant::now()));
}

/// Ends the pending start when `failed`, the LvrrState's `failed_start`, is its own: its session
/// never comes, and the next one may be another app's
pub fn note_failed(failed: Option<&(u64, String)>) {
	STARTED.with_borrow_mut(|started| started.note_failed(failed));
}

/// Notes that Steam game `id` was started outside the Library, from the Games tab. Its session may
/// come first, so a pending start of another app ends without a title, as when two apps start.
pub fn note_start_elsewhere(id: &str) {
	STARTED.with_borrow_mut(|started| started.note_start_elsewhere(id));
}

/// The app being started and how long ago
pub fn starting() -> Option<(String, Duration)> {
	STARTED.with_borrow(|started| {
		let start = started.start.as_ref()?;
		Some((start.id.clone(), start.since.elapsed()))
	})
}

/// A start is pending; cheap enough for every frame
pub fn pending() -> bool {
	STARTED.with_borrow(|started| started.start.is_some())
}

/// Ends the pending start once `clients`, the VR sessions, has one that wasn't connected before
/// it, or after the timeout. True when it ended.
pub fn resolve(clients: &[MonadoClient]) -> bool {
	STARTED.with_borrow_mut(|started| started.resolve(clients, Instant::now()))
}

/// [`resolve`] with the sessions from `list`, which runs only while a start is pending and at most
/// every [`POLL_INTERVAL`]. WayVR calls this on every frame, so a session that comes while the
/// dashboard is hidden or on another page gets its title before the start times out.
pub fn poll(list: impl FnOnce() -> Vec<MonadoClient>) {
	if !pending() {
		return;
	}
	let now = Instant::now();
	// not borrowed while `list` runs, a call back into this module would panic
	if STARTED.with_borrow_mut(|started| started.poll_due(now)) {
		let clients = list();
		STARTED.with_borrow_mut(|started| started.resolve(&clients, now));
	}
}

/// The title the Library showed for a session it started, else the name the app gave
pub fn title(client: &MonadoClient) -> String {
	STARTED.with_borrow(|started| started.title(client))
}

impl Started {
	fn note_start(&mut self, id: String, name: &str, known: Option<Vec<i64>>, generation: u64, now: Instant) {
		// nothing tells which app a session is, so while another app's start is pending neither gets
		// a title; trying the same app again keeps it
		let other = self
			.start
			.as_ref()
			.is_some_and(|start| start.id != id || start.title.is_none());
		self.start = Some(Start {
			title: (known.is_some() && !other).then(|| name.to_owned()),
			known: known.unwrap_or_default(),
			id,
			generation,
			since: now,
		});
	}

	/// A failure from before the start is an earlier try's, the start stays
	fn note_failed(&mut self, failed: Option<&(u64, String)>) {
		if let (Some(start), Some((generation, id))) = (&self.start, failed)
			&& *id == start.id
			&& *generation > start.generation
		{
			self.start = None;
		}
	}

	/// Keeps the start, its badge and timeout; the same game started again keeps its title
	fn note_start_elsewhere(&mut self, id: &str) {
		if let Some(start) = &mut self.start
			&& start.id != id
		{
			start.title = None;
		}
	}

	/// True when a start is pending and the sessions weren't listed for [`POLL_INTERVAL`]
	fn poll_due(&mut self, now: Instant) -> bool {
		let recent = self
			.polled
			.is_some_and(|polled| now.duration_since(polled) < POLL_INTERVAL);
		if self.start.is_none() || recent {
			return false;
		}
		self.polled = Some(now);
		true
	}

	fn resolve(&mut self, clients: &[MonadoClient], now: Instant) -> bool {
		let Some(start) = &self.start else {
			return false;
		};
		// given up, a session that comes later may be another app's
		if now.duration_since(start.since) >= STARTING_TIMEOUT {
			self.start = None;
			return true;
		}
		let Some(client) = clients.iter().find(|client| !start.known.contains(&client.id)) else {
			return false;
		};
		if let Some(title) = self.start.take().and_then(|start| start.title) {
			self.titles.insert(client.id, title);
		}
		true
	}

	fn title(&self, client: &MonadoClient) -> String {
		self
			.titles
			.get(&client.id)
			.cloned()
			.unwrap_or_else(|| client.name.clone())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn client(id: i64, name: &str) -> MonadoClient {
		MonadoClient {
			id,
			name: name.into(),
			is_primary: false,
			is_active: true,
			is_visible: true,
			is_focused: false,
			is_overlay: false,
			is_io_active: true,
		}
	}

	fn secs(secs: u64) -> Duration {
		Duration::from_secs(secs)
	}

	#[test]
	fn the_new_session_gets_the_title() {
		let now = Instant::now();
		let mut started = Started::default();
		started.note_start("546560".into(), "Half-Life: Alyx", Some(vec![1, 2]), 0, now);
		let running = client(2, "BeatSaber");
		assert!(!started.resolve(std::slice::from_ref(&running), now + secs(5)));
		assert!(started.start.is_some());

		let alyx = client(3, "hlvr");
		assert!(started.resolve(&[running.clone(), alyx.clone()], now + secs(20)));
		assert!(started.start.is_none());
		assert_eq!(started.title(&alyx), "Half-Life: Alyx");
		assert_eq!(started.title(&running), "BeatSaber");

		// nothing pending, a later session keeps its name
		let later = client(4, "vrchat");
		assert!(!started.resolve(&[alyx.clone(), later.clone()], now + secs(30)));
		assert_eq!(started.title(&later), "vrchat");
		assert_eq!(started.title(&alyx), "Half-Life: Alyx");
	}

	#[test]
	fn a_start_is_given_up_after_the_timeout() {
		let now = Instant::now();
		let mut started = Started::default();
		started.note_start("546560".into(), "Half-Life: Alyx", Some(Vec::new()), 0, now);
		assert!(!started.resolve(&[], now + STARTING_TIMEOUT - secs(1)));
		let late = client(1, "other");
		assert!(started.resolve(std::slice::from_ref(&late), now + STARTING_TIMEOUT));
		assert!(started.start.is_none());
		assert_eq!(started.title(&late), "other");
	}

	#[test]
	fn unsure_starts_give_no_title() {
		let now = Instant::now();
		let mut started = Started::default();

		// two apps at once: the first session may be either's
		started.note_start("546560".into(), "Half-Life: Alyx", Some(Vec::new()), 0, now);
		started.note_start("620980".into(), "Beat Saber", Some(Vec::new()), 0, now + secs(2));
		let first = client(1, "hlvr");
		assert!(started.resolve(std::slice::from_ref(&first), now + secs(10)));
		assert_eq!(started.title(&first), "hlvr");

		// the clients before the start unknown
		started.note_start("546560".into(), "Half-Life: Alyx", None, 0, now);
		let second = client(2, "hlvr");
		assert!(started.resolve(&[first.clone(), second.clone()], now + secs(10)));
		assert_eq!(started.title(&first), "hlvr");
		assert_eq!(started.title(&second), "hlvr");

		// the same app again keeps its title
		started.note_start("546560".into(), "Half-Life: Alyx", Some(vec![1, 2]), 0, now);
		started.note_start("546560".into(), "Half-Life: Alyx", Some(vec![1, 2]), 0, now + secs(15));
		let third = client(3, "hlvr");
		assert!(started.resolve(&[first, second, third.clone()], now + secs(20)));
		assert_eq!(started.title(&third), "Half-Life: Alyx");
	}

	#[test]
	fn a_start_elsewhere_gives_no_title() {
		let now = Instant::now();
		let mut started = Started::default();

		// the same game from the Games tab
		started.note_start("546560".into(), "Half-Life: Alyx", Some(vec![1]), 0, now);
		started.note_start_elsewhere("546560");
		let alyx = client(2, "hlvr");
		assert!(started.resolve(&[client(1, "vrchat"), alyx.clone()], now + secs(20)));
		assert_eq!(started.title(&alyx), "Half-Life: Alyx");

		// another game: the start stays, but the next session may be that game's
		started.note_start("546560".into(), "Half-Life: Alyx", Some(vec![1, 2]), 0, now);
		started.note_start_elsewhere("620980");
		assert!(started.start.is_some());
		let beat_saber = client(3, "BeatSaber");
		assert!(started.resolve(&[alyx, beat_saber.clone()], now + secs(40)));
		assert_eq!(started.title(&beat_saber), "BeatSaber");
	}

	#[test]
	fn a_failed_start_ends() {
		let now = Instant::now();
		let mut started = Started::default();
		started.note_start("546560".into(), "Half-Life: Alyx", Some(vec![1]), 5, now);
		// an earlier try's failure, another app's, and none
		started.note_failed(Some(&(5, "546560".into())));
		started.note_failed(Some(&(6, "620980".into())));
		started.note_failed(None);
		assert!(started.start.is_some());

		started.note_failed(Some(&(6, "546560".into())));
		assert!(started.start.is_none());
		// the next session isn't taken for it
		let vrchat = client(2, "vrchat");
		assert!(!started.resolve(std::slice::from_ref(&vrchat), now + secs(5)));
		assert_eq!(started.title(&vrchat), "vrchat");
	}

	#[test]
	fn sessions_are_listed_while_a_start_is_pending() {
		let now = Instant::now();
		let mut started = Started::default();
		assert!(!started.poll_due(now));
		started.note_start("546560".into(), "Half-Life: Alyx", Some(Vec::new()), 0, now);
		assert!(started.poll_due(now));
		assert!(!started.poll_due(now + POLL_INTERVAL / 2));
		assert!(started.poll_due(now + POLL_INTERVAL));
		// a session came
		assert!(started.resolve(&[client(1, "hlvr")], now + POLL_INTERVAL));
		assert!(!started.poll_due(now + POLL_INTERVAL * 4));
	}

	// as WayVR's tick calls it
	#[test]
	fn poll_gives_the_title() {
		let mut listed = 0;
		poll(|| {
			listed += 1;
			Vec::new()
		});
		assert_eq!(listed, 0);

		note_start("546560".into(), "Half-Life: Alyx", Some(vec![1]), 0);
		let alyx = client(2, "hlvr");
		poll(|| {
			listed += 1;
			vec![client(1, "vrchat"), alyx.clone()]
		});
		assert_eq!(listed, 1);
		assert!(!pending());
		assert_eq!(title(&alyx), "Half-Life: Alyx");
	}
}
