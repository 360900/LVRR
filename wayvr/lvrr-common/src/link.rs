//! Connection to the LVRR server on the session bus.
//!
//! D-Bus calls block, so they run on a worker thread with its own connection; the dashboard
//! polls the results from its update loop. In between, the worker sleeps until the bus or a
//! request wakes it.

use std::{
	io::{ErrorKind, Read, Write},
	os::{fd::AsRawFd, unix::net::UnixStream},
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU32, Ordering},
		mpsc::{self, Receiver, SendError, Sender, TryRecvError},
	},
	thread,
	time::{Duration, Instant},
};

use dbus::{
	arg::{PropMap, RefArg, cast},
	blocking::{
		BlockingSender, Connection,
		stdintf::org_freedesktop_dbus::{Properties, PropertiesPropertiesChanged},
	},
	channel::{BusType, Channel, MatchingReceiver},
	message::MatchRule,
};

use crate::{
	headset::{HeadsetSettings, HeadsetStats},
	state::{
		Failure, KnownKey, Link, LvrrBackend, LvrrRequest, LvrrState, Message, MessageKind, Notice, Reason, VrApplication,
	},
};

const NAME: &str = "io.github.wivrn.Server";
const PATH: &str = "/io/github/wivrn/Server";
const INTERFACE: &str = "io.github.wivrn.Server";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// ListApplications scans the Steam library and the desktop entries before it answers, which a
/// big library on a cold disk makes slow
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// The wait before the first GetAll retry, doubled per failure up to RETRY_DELAY
const FIRST_RETRY: Duration = Duration::from_millis(250);
/// How long a refused EnablePairing waits for the session to end, like `lvrr pair`
const SESSION_END_TIMEOUT: Duration = Duration::from_secs(15);
/// The wait between EnablePairing tries meanwhile
const PAIRING_RETRY: Duration = Duration::from_millis(250);

enum Event {
	Link(Link),
	Properties(Vec<Property>),
	Message(Message),
	Applications(Vec<VrApplication>),
	/// A ListApplications failed, besides its Failed; the last list stays
	ApplicationsFailed,
	/// A request the user made failed; `title` is from [`LvrrRequest::failure_key`]
	Failed {
		title: &'static str,
		failure: Failure,
	},
	/// A StartApplication failed, besides its Failed: the app with this id doesn't start
	StartFailed(String),
	/// A SetHeadsetSettings call returned, after its Properties or Failed
	HeadsetAnswered,
	/// An EnablePairing ended, after its PIN or Failed: it got one, failed or stopped waiting
	PairingDone,
}

#[allow(clippy::large_enum_variant)]
enum Property {
	HeadsetConnected(bool),
	SessionRunning(bool),
	SystemName(String),
	Bitrate(u32),
	PairingEnabled(bool),
	EncryptionEnabled(bool),
	Pin(String),
	KnownKeys(Vec<KnownKey>),
	SteamCommand(String),
	ServerConfig(serde_json::Value),
	HeadsetSettings(Option<HeadsetSettings>),
	/// None when not streaming, and 3 s after the last sample
	HeadsetStats(Option<HeadsetStats>),
}

pub struct LvrrLink {
	state: LvrrState,
	requests: Sender<LvrrRequest>,
	/// Wakes the worker for each request. Closed after `requests`, it ends the worker.
	wake: UnixStream,
	events: Receiver<Event>,
}

impl LvrrLink {
	pub fn start() -> anyhow::Result<Self> {
		Self::start_with_waker(|| {})
	}

	/// Like [`Self::start`], for a dashboard that doesn't poll every frame: the worker calls
	/// `waker` on its thread once [`LvrrBackend::poll`] has news
	pub fn start_with_waker(waker: impl Fn() + Send + 'static) -> anyhow::Result<Self> {
		let (requests, requests_rx) = mpsc::channel();
		let (events_tx, events) = mpsc::channel();
		let (wake, wake_rx) = UnixStream::pair()?;
		wake.set_nonblocking(true)?;
		wake_rx.set_nonblocking(true)?;
		let events_tx = Events {
			tx: events_tx,
			sent: Arc::default(),
		};
		thread::Builder::new()
			.name("lvrr-dbus".into())
			.spawn(move || worker(&requests_rx, &wake_rx, &events_tx, &waker))?;
		Ok(Self {
			state: LvrrState::default(),
			requests,
			wake,
			events,
		})
	}
}

impl LvrrBackend for LvrrLink {
	fn poll(&mut self) -> Vec<Notice> {
		let mut notices = Vec::new();
		while let Ok(event) = self.events.try_recv() {
			apply(&mut self.state, event, &mut notices);
		}
		notices
	}

	fn state(&self) -> &LvrrState {
		&self.state
	}

	fn request(&mut self, request: LvrrRequest) -> anyhow::Result<()> {
		match self.state.link {
			Link::Online => {}
			Link::Foreign => anyhow::bail!("Another WiVRn server is running. Quit it to use LVRR."),
			Link::NoBus | Link::ServerAbsent => anyhow::bail!("The LVRR server is not running"),
		}
		let headset = matches!(request, LvrrRequest::SetHeadsetSettings(_));
		let pairing = matches!(request, LvrrRequest::EnablePairing { .. });
		let list = matches!(request, LvrrRequest::RefreshApplications);
		self
			.requests
			.send(request)
			.map_err(|_| anyhow::anyhow!("The LVRR server link stopped"))?;
		// WouldBlock: the worker has wakes to read already
		let _ = self.wake.write(&[0]);
		if headset {
			self.state.headset_requests += 1;
		}
		// the pages show the wait, which a session ending makes seconds long
		if pairing && set(&mut self.state.pairing_pending, true) {
			self.state.changed();
		}
		// the Library shows Loading... again until the answer
		if list && set(&mut self.state.applications_failed, false) {
			self.state.changed();
		}
		Ok(())
	}
}

fn apply(state: &mut LvrrState, event: Event, notices: &mut Vec<Notice>) {
	match event {
		Event::Link(link) => state.set_link(link),
		// The signal matches follow whoever owns the name, another WiVRn server too. fetch_all
		// goes Online before its snapshot, so nothing of LVRR's server is lost.
		Event::Properties(_) | Event::Message(_) if state.link != Link::Online => {}
		Event::Properties(properties) => {
			let mut changed = false;
			for property in properties {
				changed |= match property {
					// its stream ends with it; the server also sets it false when the session ends
					Property::HeadsetConnected(v) => {
						if !v {
							state.clear_stats();
						}
						set(&mut state.headset_connected, v)
					}
					// lvrr-ui's link outlives sessions, Home lists the messages since the last one
					// started. The server reports a session before that session can send any. The
					// count tells a page one session from the next, also when it missed the end.
					Property::SessionRunning(v) => {
						if v && !state.session_running {
							state.messages.clear();
							state.sessions += 1;
						}
						set(&mut state.session_running, v)
					}
					Property::SystemName(v) => set(&mut state.system_name, v),
					Property::Bitrate(v) => set(&mut state.bitrate_bps, v),
					Property::PairingEnabled(v) => set(&mut state.pairing_enabled, v),
					Property::EncryptionEnabled(v) => set(&mut state.encryption_enabled, v),
					Property::Pin(v) => set(&mut state.pin, v),
					Property::KnownKeys(v) => set(&mut state.known_keys, v),
					Property::SteamCommand(v) => set(&mut state.steam_command, v),
					Property::ServerConfig(v) => set(&mut state.server_config, v),
					// the PropertiesChanged after a SetHeadsetSettings reply repeats it, while
					// each headset answer has a new request_id
					Property::HeadsetSettings(v) => set(&mut state.headset, v),
					// statistics have their own generation
					Property::HeadsetStats(Some(v)) => {
						state.push_stats(v);
						false
					}
					// "" also once the headset sent none for 3 s, as it sleeps, shows its system menu or
					// waits out a stall: PC > Performance keeps its graphs until it disconnects
					Property::HeadsetStats(None) => {
						if state.headset_connected {
							// stats_generation stays: PC > Performance reads each step as a sample or a clearing
							state.stats_live = false;
						} else {
							state.clear_stats();
						}
						false
					}
				};
			}
			if changed {
				state.changed();
			}
		}
		Event::Message(message) => {
			state.push_message(message.clone());
			notices.push(Notice::Message(message));
		}
		Event::Applications(applications) => {
			state.applications = Some(applications);
			state.applications_failed = false;
			state.changed();
		}
		Event::ApplicationsFailed => {
			if set(&mut state.applications_failed, true) {
				state.changed();
			}
		}
		Event::Failed { title, failure } => {
			notices.push(Notice::RequestFailed { title, failure });
			// the pages show the server's values again, without the change that failed
			state.changed();
		}
		Event::StartFailed(id) => {
			state.changed();
			state.failed_start = Some((state.generation, id));
		}
		Event::HeadsetAnswered => {
			state.headset_requests = state.headset_requests.saturating_sub(1);
			// even unchanged or failed, so the Headset page drops a value the headset didn't take
			state.changed();
		}
		Event::PairingDone => {
			if set(&mut state.pairing_pending, false) {
				state.changed();
			}
		}
	}
}

/// Sets `field` to `value`, true if that changed it
fn set<T: PartialEq>(field: &mut T, value: T) -> bool {
	let changed = *field != value;
	*field = value;
	changed
}

/// The worker's side of the events, which notes when the dashboard has news to be woken for
#[derive(Clone)]
struct Events {
	tx: Sender<Event>,
	/// Some were sent since the dashboard was last woken
	sent: Arc<AtomicBool>,
}

impl Events {
	fn send(&self, event: Event) -> Result<(), SendError<Event>> {
		self.tx.send(event)?;
		self.sent.store(true, Ordering::Relaxed);
		Ok(())
	}

	/// Calls `waker` if events were sent since the last time
	fn wake(&self, waker: &dyn Fn()) {
		if self.sent.swap(false, Ordering::Relaxed) {
			waker();
		}
	}
}

fn worker(requests: &Receiver<LvrrRequest>, wake: &UnixStream, events: &Events, waker: &dyn Fn()) {
	loop {
		match connect() {
			Ok(connection) => match run(&connection, requests, wake, events, waker) {
				Ok(()) => return, // the dashboard is gone
				Err(e) => log::warn!("LVRR server link lost: {e:?}"),
			},
			Err(e) => log::warn!("No session bus for the LVRR server link: {e:?}"),
		}
		if events.send(Event::Link(Link::NoBus)).is_err() {
			return;
		}
		events.wake(waker);
		thread::sleep(RETRY_DELAY);
	}
}

/// A session bus connection whose socket the worker can wait on
fn connect() -> Result<Connection, dbus::Error> {
	let mut channel = Channel::get_private(BusType::Session)?;
	channel.set_watch_enabled(true);
	Ok(Connection::from(channel))
}

fn run(
	connection: &Connection,
	requests: &Receiver<LvrrRequest>,
	wake: &UnixStream,
	events: &Events,
	waker: &dyn Fn(),
) -> anyhow::Result<()> {
	// Signal callbacks can't call the server themselves, they leave it to the loop below
	let refresh = Arc::new(AtomicBool::new(true));
	// The name got a new owner or lost it: a retry for the last owner is moot
	let owner_changed = Arc::new(AtomicBool::new(false));
	// The serial of the last PropertiesChanged with HeadsetSettings, which call() compares with a
	// SetHeadsetSettings reply's
	let headset_signal = Arc::new(AtomicU32::new(0));

	connection.add_match(
		MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged")
			.with_sender(NAME)
			.with_path(PATH),
		{
			let events = events.clone();
			let refresh = refresh.clone();
			let headset_signal = headset_signal.clone();
			move |signal: PropertiesPropertiesChanged, _: &Connection, message: &dbus::Message| {
				if signal.interface_name == INTERFACE {
					if signal.changed_properties.contains_key("HeadsetSettings") {
						headset_signal.store(message.get_serial().unwrap_or(0), Ordering::Relaxed);
					}
					let _ = events.send(Event::Properties(decode(&signal.changed_properties)));
					if !signal.invalidated_properties.is_empty() {
						refresh.store(true, Ordering::Relaxed);
					}
				}
				true
			}
		},
	)?;

	connection.add_match(
		MatchRule::new_signal(INTERFACE, "HeadsetMessage")
			.with_sender(NAME)
			.with_path(PATH),
		{
			let events = events.clone();
			move |(kind, text): (String, String), _: &Connection, _: &dbus::Message| {
				let kind = match kind.as_str() {
					"toast_urgent" => MessageKind::Urgent,
					"error" => MessageKind::Error,
					_ => MessageKind::Info,
				};
				let _ = events.send(Event::Message(Message { kind, text }));
				true
			}
		},
	)?;

	connection.add_match(
		MatchRule::new_signal(INTERFACE, "ServerError")
			.with_sender(NAME)
			.with_path(PATH),
		{
			let events = events.clone();
			move |(place, text): (String, String), _: &Connection, _: &dbus::Message| {
				let _ = events.send(Event::Message(Message {
					kind: MessageKind::Error,
					text: format!("{place}: {text}"),
				}));
				true
			}
		},
	)?;

	// The server's name only, or every client that comes or goes on the bus would wake the worker.
	// dbus-rs's MatchRule has no arg0, so the bus gets the rule as text.
	let rule = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged").with_sender("org.freedesktop.DBus");
	connection.add_match_no_cb(&format!("{},arg0='{NAME}'", rule.match_str()))?;
	connection.start_receive(rule, {
		let events = events.clone();
		let refresh = refresh.clone();
		let owner_changed = owner_changed.clone();
		Box::new(move |message: dbus::Message, _: &Connection| {
			if let Ok((name, _old, new)) = message.read3::<String, String, String>()
				&& name == NAME
			{
				owner_changed.store(true, Ordering::Relaxed);
				if new.is_empty() {
					let _ = events.send(Event::Link(Link::ServerAbsent));
				} else {
					refresh.store(true, Ordering::Relaxed);
				}
			}
			true
		})
	});

	// After a GetAll the name's owner didn't answer: when to ask again, and the delay until then
	let mut retry: Option<(Instant, Duration)> = None;
	// An EnablePairing without its PIN yet
	let mut pairing: Option<Pairing> = None;

	loop {
		// Signals read meanwhile, such as while a call waited for its reply, wait in libdbus's
		// queue, where the socket doesn't show them
		while connection.process(Duration::ZERO)? {}
		// A call below may read more, so the loop drains again before it sleeps
		let mut called = false;

		if owner_changed.swap(false, Ordering::Relaxed) {
			// a new owner is asked at once and backs off from FIRST_RETRY, a lost one needs nothing
			retry = None;
			// nor a PIN from the last one
			end_pairing(&mut pairing, events);
		}
		let due = retry.is_some_and(|(at, _)| at <= Instant::now());
		if refresh.swap(false, Ordering::Relaxed) || due {
			called = true;
			retry = fetch_all(connection, events).err().map(|e| {
				let last = retry.map(|(_, delay)| delay);
				let delay = last.map_or(FIRST_RETRY, |last| (last * 2).min(RETRY_DELAY));
				// The first tries after a server starts often fail. One that still fails at the
				// longest delay gets a warning, once.
				if delay == RETRY_DELAY && last != Some(RETRY_DELAY) {
					log::warn!("LVRR server not answering, trying again every {RETRY_DELAY:?}: {e}");
				} else {
					log::debug!("LVRR server not answering yet: {e}");
				}
				(Instant::now() + delay, delay)
			});
		}

		loop {
			match requests.try_recv() {
				Ok(request) => {
					called = true;
					call(connection, request, events, &mut pairing, &headset_signal);
				}
				Err(TryRecvError::Empty) => break,
				Err(TryRecvError::Disconnected) => return Ok(()),
			}
		}

		if pairing.as_ref().is_some_and(|p| p.next_try <= Instant::now()) {
			called = true;
			try_pairing(connection, &mut pairing, events);
		}

		events.wake(waker);
		if called {
			continue;
		}
		let channel = connection.channel();
		let mut bus = libc::pollfd {
			fd: channel.watch().fd,
			events: libc::POLLIN,
			revents: 0,
		};
		// what a full socket kept back goes once it takes more
		if channel.has_messages_to_send() {
			bus.events |= libc::POLLOUT;
		}
		let next_try = pairing.as_ref().map(|p| p.next_try);
		// the next GetAll retry or EnablePairing try, whichever comes first
		let until = retry.map(|(at, _)| at).into_iter().chain(next_try).min();
		if !wait(bus, wake, until)? {
			return Ok(());
		}
	}
}

/// Sleeps until `bus`, the bus socket, is ready, a request comes or `until`. False once the
/// dashboard dropped the link.
fn wait(bus: libc::pollfd, mut wake: &UnixStream, until: Option<Instant>) -> std::io::Result<bool> {
	let mut fds = [
		bus,
		libc::pollfd {
			fd: wake.as_raw_fd(),
			events: libc::POLLIN,
			revents: 0,
		},
	];
	loop {
		// rounded up, or it wakes just before `until` with nothing due yet
		let timeout = until.map_or(-1, |until| {
			let left = until.saturating_duration_since(Instant::now());
			i32::try_from(left.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX)
		});
		// SAFETY: `fds` holds valid pollfd entries, poll() writes only their revents
		if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } >= 0 {
			break;
		}
		let error = std::io::Error::last_os_error();
		if error.kind() != ErrorKind::Interrupted {
			return Err(error);
		}
	}
	if fds[1].revents == 0 {
		return Ok(true);
	}
	// a byte per request, which the loop handles or already did
	let mut bytes = [0; 64];
	loop {
		match wake.read(&mut bytes) {
			Ok(0) => return Ok(false),
			Ok(_) => {}
			Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(true),
			Err(e) => return Err(e),
		}
	}
}

/// Asks the server for every property. Fails when the name has an owner that didn't answer,
/// which is worth asking again: a starting server owns its name before it exports its object,
/// and a busy one may not answer in time.
fn fetch_all(connection: &Connection, events: &Events) -> Result<(), dbus::Error> {
	let proxy = connection.with_proxy(NAME, PATH, CALL_TIMEOUT);
	match proxy.get_all(INTERFACE) {
		Ok(properties) if !lvrr_server(&properties) => {
			log::info!("Another WiVRn server owns {NAME}, leaving it alone");
			let _ = events.send(Event::Link(Link::Foreign));
		}
		Ok(properties) => {
			let _ = events.send(Event::Link(Link::Online));
			let _ = events.send(Event::Properties(decode(&properties)));
		}
		Err(e) => {
			// requests stay refused until a GetAll succeeds
			let _ = events.send(Event::Link(Link::ServerAbsent));
			if !no_owner(&e) {
				return Err(e);
			}
			log::debug!("LVRR server not running: {e}");
		}
	}
	Ok(())
}

/// The bus's own answer for a name without owner, rather than an owner that didn't answer
fn no_owner(error: &dbus::Error) -> bool {
	matches!(
		error.name(),
		Some("org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner")
	)
}

/// Why a call failed: the server's documented errors, and the bus's when the server didn't answer
fn failure_of(error: &dbus::Error) -> Failure {
	let reason = match error.name() {
		Some("io.github.wivrn.Server.Timeout") => Reason::HeadsetSlow,
		Some("io.github.wivrn.Server.NotConnected") => Reason::NoHeadset,
		Some("io.github.wivrn.Server.NoSession") => Reason::NoSession,
		// the server hangs or quit, which libdbus explains at length
		Some(
			"org.freedesktop.DBus.Error.NoReply"
			| "org.freedesktop.DBus.Error.Timeout"
			| "org.freedesktop.DBus.Error.Disconnected",
		) => Reason::ServerGone,
		_ if no_owner(error) => Reason::ServerGone,
		_ => return Failure::Other(error.message().unwrap_or("unknown error").to_string()),
	};
	Failure::Known(reason)
}

/// LVRR's server always has HeadsetSettings, "" without a headset; upstream WiVRn and its
/// Flatpak don't. Asking the API needs no install path, and a server with file capabilities
/// hides its executable.
fn lvrr_server(properties: &PropMap) -> bool {
	properties.contains_key("HeadsetSettings")
}

/// Whether the message with serial `a` was sent after the one with `b`, both from one sender, which
/// numbers its messages in the order it sends them, wrapping around. 0 is none.
fn sent_after(a: u32, b: u32) -> bool {
	a != 0 && (a.wrapping_sub(b) as i32).is_positive()
}

fn call(
	connection: &Connection,
	request: LvrrRequest,
	events: &Events,
	pairing: &mut Option<Pairing>,
	headset_signal: &AtomicU32,
) {
	let proxy = connection.with_proxy(NAME, PATH, CALL_TIMEOUT);
	let name = request.name();
	let title = request.failure_key();
	let headset = matches!(request, LvrrRequest::SetHeadsetSettings(_));
	let quit = matches!(request, LvrrRequest::Quit);
	let result: Result<(), dbus::Error> = match request {
		// as proxy.method_call(), keeping the reply's serial
		LvrrRequest::SetHeadsetSettings(patch) => {
			let message = dbus::Message::method_call(&proxy.destination, &proxy.path, &INTERFACE.into(), &name.into())
				.append1(patch.to_string());
			connection
				.send_with_reply_and_block(message, CALL_TIMEOUT)
				.and_then(|reply| {
					let settings: String = reply.read1()?;
					// The server sends each answer again in a PropertiesChanged after the reply. One for an
					// earlier call may be queued since: dispatched after this reply, it would undo it.
					headset_signal.store(0, Ordering::Relaxed);
					while connection.process(Duration::ZERO).unwrap_or(false) {}
					// The drain also reads what came after the reply. A signal sent later has the value then:
					// this reply's, or a newer one, such as the answer to another dashboard.
					let signal = headset_signal.swap(0, Ordering::Relaxed);
					if !reply.get_serial().is_some_and(|serial| sent_after(signal, serial)) {
						let _ = events.send(Event::Properties(vec![Property::HeadsetSettings(parse(&settings))]));
					}
					Ok(())
				})
		}
		LvrrRequest::RestartSession | LvrrRequest::StopSession | LvrrRequest::Disconnect | LvrrRequest::Quit => {
			proxy.method_call(INTERFACE, name, ())
		}
		// run() asks for the PIN, again until the session ended; one more meanwhile joins it
		LvrrRequest::EnablePairing { timeout_secs } => {
			let now = Instant::now();
			pairing.get_or_insert(Pairing {
				timeout_secs,
				title,
				deadline: now + SESSION_END_TIMEOUT,
				next_try: now,
			});
			Ok(())
		}
		LvrrRequest::DisablePairing => {
			// also stops waiting for a PIN
			end_pairing(pairing, events);
			proxy.method_call(INTERFACE, name, ())
		}
		LvrrRequest::RevokeKey(key) => proxy.method_call(INTERFACE, name, (key,)),
		LvrrRequest::RenameKey { key, name: new_name } => proxy.method_call(INTERFACE, name, (key, new_name)),
		LvrrRequest::SetServerConfig(config) => proxy.set(INTERFACE, "JsonConfiguration", config.to_string()),
		LvrrRequest::RefreshApplications => {
			connection
				.with_proxy(NAME, PATH, LIST_TIMEOUT)
				.method_call(INTERFACE, name, ())
				.map(|(applications,): (Vec<(String, String, String)>,)| {
					let applications = applications
						.into_iter()
						.map(|(id, name, icon)| VrApplication { id, name, icon })
						.collect();
					let _ = events.send(Event::Applications(applications));
				})
				// the Library says so without a list, the toast says why
				.inspect_err(|_| {
					let _ = events.send(Event::ApplicationsFailed);
				})
		}
		LvrrRequest::StartApplication(id) => {
			proxy
				.method_call(INTERFACE, name, (id.as_str(),))
				// the Library ends the start it shows, the toast says why
				.inspect_err(move |_| {
					let _ = events.send(Event::StartFailed(id));
				})
		}
		LvrrRequest::StopApplication(id) => proxy.method_call(INTERFACE, name, (id,)),
		LvrrRequest::SetDashboardVisible(visible) => proxy.method_call(INTERFACE, name, (visible,)),
	};

	let failure = match result {
		Ok(()) => None,
		Err(e) => match failure_of(&e) {
			// the server may exit before it answers, `lvrr stop` ignores that too
			Failure::Known(Reason::ServerGone) if quit => None,
			failure => {
				log::warn!("{name} failed: {e:?}");
				Some(failure)
			}
		},
	};
	// a background report is only logged
	if let (Some(title), Some(failure)) = (title, failure) {
		let _ = events.send(Event::Failed { title, failure });
	}
	if headset {
		let _ = events.send(Event::HeadsetAnswered);
	}
}

/// An EnablePairing until it has a PIN. The server refuses one, with an empty PIN, while a session
/// runs or a headset connects; it is asked again once the session ended, as `lvrr pair` does.
struct Pairing {
	timeout_secs: i32,
	/// What a failure says failed, from [`LvrrRequest::failure_key`]
	title: Option<&'static str>,
	/// When it gives up
	deadline: Instant,
	next_try: Instant,
}

/// Asks for the waiting EnablePairing's PIN, unless the session still runs
fn try_pairing(connection: &Connection, pairing: &mut Option<Pairing>, events: &Events) {
	let Some(waiting) = pairing.as_mut() else {
		return;
	};
	let timeout_secs = waiting.timeout_secs;
	let proxy = connection.with_proxy(NAME, PATH, CALL_TIMEOUT);
	// not asked during the session, the server would log each refusal
	let pin = proxy.get::<bool>(INTERFACE, "SessionRunning").and_then(|running| {
		if running {
			return Ok(String::new());
		}
		proxy
			.method_call(INTERFACE, "EnablePairing", (timeout_secs,))
			.map(|(pin,): (String,)| pin)
	});
	let failure = match pin {
		Ok(pin) if !pin.is_empty() => {
			// Sent with the wait's end, the PIN replaces it on the page; the PropertiesChanged after
			// the answer repeats it. Signals from before the answer go first, such as a DisablePairing's.
			while connection.process(Duration::ZERO).unwrap_or(false) {}
			let _ = events.send(Event::Properties(vec![
				Property::PairingEnabled(true),
				Property::Pin(pin),
			]));
			None
		}
		Ok(_) if Instant::now() < waiting.deadline => {
			waiting.next_try = Instant::now() + PAIRING_RETRY;
			return;
		}
		Ok(_) => {
			log::warn!("EnablePairing still refused after {SESSION_END_TIMEOUT:?}, the session didn't end");
			Some(Failure::Known(Reason::Busy))
		}
		Err(e) => {
			log::warn!("EnablePairing failed: {e:?}");
			Some(failure_of(&e))
		}
	};
	if let (Some(title), Some(failure)) = (waiting.title, failure) {
		let _ = events.send(Event::Failed { title, failure });
	}
	end_pairing(pairing, events);
}

/// Stops waiting for a PIN, if an EnablePairing waits
fn end_pairing(pairing: &mut Option<Pairing>, events: &Events) {
	if pairing.take().is_some() {
		let _ = events.send(Event::PairingDone);
	}
}

fn parse<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
	if text.is_empty() {
		return None;
	}
	serde_json::from_str(text)
		.inspect_err(|e| log::warn!("Invalid JSON from the LVRR server: {e}"))
		.ok()
}

fn decode(properties: &PropMap) -> Vec<Property> {
	let mut out = Vec::new();
	for (key, value) in properties {
		let value = &*value.0;
		let string = || cast::<String>(value).cloned();
		let boolean = || cast::<bool>(value).copied();
		let property = match key.as_str() {
			"HeadsetConnected" => boolean().map(Property::HeadsetConnected),
			"SessionRunning" => boolean().map(Property::SessionRunning),
			"PairingEnabled" => boolean().map(Property::PairingEnabled),
			"EncryptionEnabled" => boolean().map(Property::EncryptionEnabled),
			"SystemName" => string().map(Property::SystemName),
			"Pin" => string().map(Property::Pin),
			"SteamCommand" => string().map(Property::SteamCommand),
			"Bitrate" => cast::<u32>(value).copied().map(Property::Bitrate),
			"JsonConfiguration" => string().map(|s| Property::ServerConfig(parse(&s).unwrap_or_default())),
			"HeadsetSettings" => string().map(|s| Property::HeadsetSettings(parse(&s))),
			// "" when not streaming. A sample that doesn't parse is dropped, it mustn't clear the history.
			"HeadsetStats" => string().and_then(|s| match s.as_str() {
				"" => Some(Property::HeadsetStats(None)),
				sample => parse(sample).map(|v| Property::HeadsetStats(Some(v))),
			}),
			"KnownKeys" => Some(Property::KnownKeys(known_keys(value))),
			_ => None,
		};
		out.extend(property);
	}
	out
}

/// a(ssx): name, public key, last connection or 0
fn known_keys(value: &dyn RefArg) -> Vec<KnownKey> {
	let Some(items) = value.as_iter() else {
		return Vec::new();
	};
	items
		.filter_map(|item| {
			let mut fields = item.as_iter()?;
			let name = fields.next()?.as_str()?.to_string();
			let public_key = fields.next()?.as_str()?.to_string();
			let last_connection = fields.next()?.as_i64().filter(|t| *t != 0);
			Some(KnownKey {
				name,
				public_key,
				last_connection,
			})
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn answer(request_id: u32) -> Event {
		Event::Properties(vec![Property::HeadsetSettings(Some(HeadsetSettings {
			request_id,
			..Default::default()
		}))])
	}

	fn online() -> LvrrState {
		LvrrState {
			link: Link::Online,
			..Default::default()
		}
	}

	/// A link without its worker, and the worker's ends: the events it sends, the requests and the
	/// wakes it reads. Bind them to names, not `_`, which drops them: request() fails without the
	/// requests' end.
	fn test_link(link: Link) -> (LvrrLink, Sender<Event>, Receiver<LvrrRequest>, UnixStream) {
		let (requests, worker_requests) = mpsc::channel();
		let (events_tx, events) = mpsc::channel();
		let (wake, worker_wake) = UnixStream::pair().unwrap();
		// as start_with_waker() does, so wait() reads every wake and returns
		wake.set_nonblocking(true).unwrap();
		worker_wake.set_nonblocking(true).unwrap();
		let link = LvrrLink {
			state: LvrrState {
				link,
				..Default::default()
			},
			requests,
			wake,
			events,
		};
		(link, events_tx, worker_requests, worker_wake)
	}

	// The PropertiesChanged after a SetHeadsetSettings reply carries the same settings
	#[test]
	fn applies_each_headset_answer_once() {
		let mut state = online();
		apply(&mut state, answer(1), &mut Vec::new());
		let generation = state.generation;
		apply(&mut state, answer(1), &mut Vec::new());
		assert_eq!(state.generation, generation);
		apply(&mut state, answer(2), &mut Vec::new());
		assert_eq!(state.generation, generation + 1);
	}

	// A SetHeadsetSettings reply is dropped once its drain dispatched a HeadsetSettings signal the
	// server sent after it, which has the newer value
	#[test]
	fn tells_a_later_signal() {
		assert!(sent_after(8, 7));
		assert!(!sent_after(7, 7));
		assert!(!sent_after(6, 7));
		// no signal
		assert!(!sent_after(0, 7));
		assert!(!sent_after(0, u32::MAX));
		// across the wrap
		assert!(sent_after(1, u32::MAX));
		assert!(!sent_after(u32::MAX, 1));
	}

	// The server's HeadsetStats "" pauses the samples while the headset stays connected, the stream
	// ends with the connection. A sample that doesn't parse leaves the history.
	#[test]
	fn clears_stats_when_the_stream_ends() {
		let stats = |json: &str| {
			let mut properties = PropMap::new();
			properties.insert("HeadsetStats".into(), dbus::arg::Variant(Box::new(json.to_string())));
			Event::Properties(decode(&properties))
		};
		let connected = |v| Event::Properties(vec![Property::HeadsetConnected(v)]);
		let streaming = || {
			let mut state = LvrrState {
				headset_connected: true,
				..online()
			};
			apply(&mut state, stats("{}"), &mut Vec::new());
			state
		};
		let mut state = streaming();
		apply(&mut state, stats("{"), &mut Vec::new());
		assert_eq!(state.stats.len(), 1);
		assert!(state.latest_stats().is_some());
		assert_eq!(state.stats_generation, 1);

		// the headset shows its system menu: no current sample, the same history
		apply(&mut state, stats(""), &mut Vec::new());
		assert_eq!(state.stats.len(), 1);
		assert!(state.latest_stats().is_none());
		assert_eq!(state.stats_generation, 1);
		apply(&mut state, stats("{}"), &mut Vec::new());
		assert_eq!(state.stats.len(), 2);
		assert!(state.latest_stats().is_some());
		assert_eq!(state.stats_generation, 2);
		// no page rebuilds for statistics, PC > Performance follows stats_generation
		assert_eq!(state.generation, 0);

		apply(&mut state, connected(false), &mut Vec::new());
		assert!(state.stats.is_empty() && state.latest_stats().is_none());
		assert_eq!(state.stats_generation, 3);
		// nothing left to clear, like the "" GetAll returns before a headset streams
		apply(&mut state, stats(""), &mut Vec::new());
		assert_eq!(state.stats_generation, 3);

		// a disconnect, also the session's end, sends both in one signal, in any order
		for properties in [
			vec![Property::HeadsetStats(None), Property::HeadsetConnected(false)],
			vec![Property::HeadsetConnected(false), Property::HeadsetStats(None)],
		] {
			let mut state = streaming();
			apply(&mut state, Event::Properties(properties), &mut Vec::new());
			assert!(state.stats.is_empty() && state.latest_stats().is_none());
		}
	}

	// lvrr-ui's link outlives sessions, Home lists the messages since the last one started
	#[test]
	fn clears_messages_when_a_session_starts() {
		let mut state = online();
		let message = |text: &str| {
			Event::Message(Message {
				kind: MessageKind::Error,
				text: text.into(),
			})
		};
		let running = |v| Event::Properties(vec![Property::SessionRunning(v)]);
		apply(&mut state, message("last session"), &mut Vec::new());
		let generation = state.generation;
		apply(&mut state, running(true), &mut Vec::new());
		assert!(state.messages.is_empty());
		assert_eq!(state.generation, generation + 1);
		assert_eq!(state.sessions, 1);

		// a GetAll repeats it while the session runs
		apply(&mut state, message("this session"), &mut Vec::new());
		apply(&mut state, running(true), &mut Vec::new());
		assert_eq!(state.sessions, 1);
		// once it ended, and while the server is gone, they may say why
		apply(&mut state, running(false), &mut Vec::new());
		apply(&mut state, Event::Link(Link::ServerAbsent), &mut Vec::new());
		assert_eq!(state.messages.len(), 1);
		assert_eq!(state.messages[0].text, "this session");

		// the next server's GetAll, during its first session
		apply(&mut state, Event::Link(Link::Online), &mut Vec::new());
		apply(&mut state, running(true), &mut Vec::new());
		assert!(state.messages.is_empty());
		assert_eq!(state.sessions, 2);
	}

	// The Headset page waits until every change it sent is answered
	#[test]
	fn counts_headset_requests() {
		let (mut link, answers, worker, _wake) = test_link(Link::Online);
		let settings = || LvrrRequest::SetHeadsetSettings(serde_json::json!({"codec": "h265"}));
		link.request(settings()).unwrap();
		link.request(settings()).unwrap();
		link.request(LvrrRequest::RefreshApplications).unwrap();
		assert_eq!(link.state().headset_requests, 2);

		// an answer that changed nothing still shows the page again
		let generation = link.state().generation;
		answers.send(Event::HeadsetAnswered).unwrap();
		link.poll();
		assert_eq!(link.state().headset_requests, 1);
		assert_eq!(link.state().generation, generation + 1);

		// never sent, never answered
		drop(worker);
		assert!(link.request(settings()).is_err());
		assert_eq!(link.state().headset_requests, 1);

		// forgotten with the link, so an answer after it can't underflow
		answers.send(Event::Link(Link::ServerAbsent)).unwrap();
		answers.send(Event::HeadsetAnswered).unwrap();
		link.poll();
		assert_eq!(link.state().headset_requests, 0);
	}

	#[test]
	fn tells_lvrr_server_from_upstream() {
		let mut properties = PropMap::new();
		properties.insert("PairingEnabled".into(), dbus::arg::Variant(Box::new(false)));
		assert!(!lvrr_server(&properties));
		properties.insert("HeadsetSettings".into(), dbus::arg::Variant(Box::new(String::new())));
		assert!(lvrr_server(&properties));
	}

	// Only a name without owner ends the GetAll retries
	#[test]
	fn tells_no_owner_from_no_answer() {
		let error = |name: &str| dbus::Error::new_custom(name, "");
		assert!(no_owner(&error("org.freedesktop.DBus.Error.ServiceUnknown")));
		assert!(no_owner(&error("org.freedesktop.DBus.Error.NameHasNoOwner")));
		// GDBus's answer before the server exports its object, and a call that timed out
		assert!(!no_owner(&error("org.freedesktop.DBus.Error.UnknownMethod")));
		assert!(!no_owner(&error("org.freedesktop.DBus.Error.NoReply")));
	}

	// The documented errors, and a server that didn't answer, are told in the dashboard's words
	#[test]
	fn explains_failures() {
		let failure = |name: &str| failure_of(&dbus::Error::new_custom(name, "unknown key"));
		assert_eq!(
			failure("io.github.wivrn.Server.Timeout"),
			Failure::Known(Reason::HeadsetSlow)
		);
		assert_eq!(
			failure("io.github.wivrn.Server.NotConnected"),
			Failure::Known(Reason::NoHeadset)
		);
		assert_eq!(
			failure("io.github.wivrn.Server.NoSession"),
			Failure::Known(Reason::NoSession)
		);
		// a call past CALL_TIMEOUT, and one to a server that quit
		assert_eq!(
			failure("org.freedesktop.DBus.Error.NoReply"),
			Failure::Known(Reason::ServerGone)
		);
		assert_eq!(
			failure("org.freedesktop.DBus.Error.ServiceUnknown"),
			Failure::Known(Reason::ServerGone)
		);
		assert_eq!(
			failure("io.github.wivrn.Server.InvalidSettings"),
			Failure::Other("unknown key".into())
		);
	}

	// A page drops a change that failed and shows the server's values again
	#[test]
	fn failure_shows_the_pages_again() {
		let mut state = online();
		let mut notices = Vec::new();
		let failed = Event::Failed {
			title: "LVRR.FAILED.RENAME",
			failure: Failure::Known(Reason::ServerGone),
		};
		apply(&mut state, failed, &mut notices);
		assert_eq!(state.generation, 1);
		assert!(matches!(
			&notices[..],
			[Notice::RequestFailed {
				title: "LVRR.FAILED.RENAME",
				..
			}]
		));
	}

	// The Library ends its start once the generation it saw at the click is older than the failure.
	// Also without a link, which may be why the start failed.
	#[test]
	fn records_a_failed_start() {
		let mut state = LvrrState::default();
		let clicked = state.generation;
		apply(&mut state, Event::StartFailed("620980".into()), &mut Vec::new());
		assert_eq!(state.failed_start, Some((state.generation, "620980".into())));
		assert!(state.generation > clicked);
	}

	// Another WiVRn server's signals reach the same matches; its PIN isn't LVRR's
	#[test]
	fn leaves_another_server_alone() {
		let (mut link, events_tx, _worker, _wake) = test_link(Link::NoBus);
		for event in [
			Event::Link(Link::Foreign),
			Event::Properties(vec![Property::PairingEnabled(true), Property::Pin("482193".into())]),
			Event::Message(Message {
				kind: MessageKind::Info,
				text: "Hello".into(),
			}),
		] {
			events_tx.send(event).unwrap();
		}
		assert!(link.poll().is_empty());
		assert!(!link.state().pairing_enabled && link.state().pin.is_empty());
		assert!(link.state().messages.is_empty());
		let error = link.request(LvrrRequest::DisablePairing).unwrap_err();
		assert!(error.to_string().starts_with("Another WiVRn server"));
	}

	// Pair headset shows the wait from the request until the worker ends it, or the server leaves
	#[test]
	fn tracks_a_pairing() {
		let (mut link, events_tx, _worker, _wake) = test_link(Link::Online);
		let pair = || LvrrRequest::EnablePairing { timeout_secs: 120 };
		link.request(pair()).unwrap();
		assert!(link.state().pairing_pending);
		assert_eq!(link.state().generation, 1);
		// the worker sends the PIN first, so the page goes from the wait to it
		for event in [
			Event::Properties(vec![Property::PairingEnabled(true), Property::Pin("482193".into())]),
			Event::PairingDone,
		] {
			events_tx.send(event).unwrap();
		}
		link.poll();
		assert!(!link.state().pairing_pending && link.state().pin == "482193");
		assert_eq!(link.state().generation, 3);

		link.request(pair()).unwrap();
		events_tx.send(Event::Link(Link::ServerAbsent)).unwrap();
		link.poll();
		assert!(!link.state().pairing_pending);
		// one from before the loss changes nothing
		let generation = link.state().generation;
		events_tx.send(Event::PairingDone).unwrap();
		link.poll();
		assert_eq!(link.state().generation, generation);
	}

	// A failed ListApplications keeps the last list; without one the Library says so until Refresh
	#[test]
	fn tracks_a_failed_list() {
		let (mut link, events_tx, _worker, _wake) = test_link(Link::Online);
		let apps = vec![VrApplication {
			id: "620980".into(),
			name: "Beat Saber".into(),
			icon: String::new(),
		}];
		for event in [Event::Applications(apps.clone()), Event::ApplicationsFailed] {
			events_tx.send(event).unwrap();
		}
		link.poll();
		assert!(link.state().applications_failed);
		assert_eq!(link.state().applications.as_ref(), Some(&apps));

		// Refresh shows Loading... again, once
		let generation = link.state().generation;
		link.request(LvrrRequest::RefreshApplications).unwrap();
		assert!(!link.state().applications_failed);
		assert_eq!(link.state().generation, generation + 1);
		link.request(LvrrRequest::RefreshApplications).unwrap();
		assert_eq!(link.state().generation, generation + 1);

		// an answer clears it too
		for event in [Event::ApplicationsFailed, Event::Applications(Vec::new())] {
			events_tx.send(event).unwrap();
		}
		link.poll();
		assert!(!link.state().applications_failed);
		assert_eq!(link.state().applications, Some(Vec::new()));
	}

	// Each request wakes the worker's wait, which reads every wake; dropping the link ends it
	#[test]
	fn wakes_the_worker() {
		let (mut link, _events, _worker, worker_wake) = test_link(Link::Online);
		// no bus here, poll() skips a negative fd
		let bus = libc::pollfd {
			fd: -1,
			events: 0,
			revents: 0,
		};
		let soon = Some(Instant::now() + Duration::from_millis(10));
		assert!(wait(bus, &worker_wake, soon).unwrap());

		let refresh = || LvrrRequest::RefreshApplications;
		link.request(refresh()).unwrap();
		link.request(refresh()).unwrap();
		assert_eq!((&worker_wake).read(&mut [0; 4]).unwrap(), 2);
		link.request(refresh()).unwrap();
		link.request(refresh()).unwrap();
		let later = Some(Instant::now() + Duration::from_secs(5));
		assert!(wait(bus, &worker_wake, later).unwrap());
		assert_eq!((&worker_wake).read(&mut [0]).unwrap_err().kind(), ErrorKind::WouldBlock);

		drop(link);
		assert!(!wait(bus, &worker_wake, later).unwrap());
	}
}
