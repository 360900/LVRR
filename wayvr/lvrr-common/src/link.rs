//! Connection to the LVRR server on the session bus.
//!
//! D-Bus calls block, so they run on a worker thread with its own connection; the dashboard
//! polls the results from its update loop.

use std::{
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
		mpsc::{self, Receiver, Sender, TryRecvError},
	},
	thread,
	time::Duration,
};

use dbus::{
	arg::{PropMap, RefArg, cast},
	blocking::{
		Connection,
		stdintf::org_freedesktop_dbus::{Properties, PropertiesPropertiesChanged},
	},
	message::MatchRule,
};

use crate::{
	headset::{HeadsetSettings, HeadsetStats},
	state::{KnownKey, Link, LvrrBackend, LvrrRequest, LvrrState, Message, MessageKind, Notice, VrApplication},
};

const NAME: &str = "io.github.wivrn.Server";
const PATH: &str = "/io/github/wivrn/Server";
const INTERFACE: &str = "io.github.wivrn.Server";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_DELAY: Duration = Duration::from_secs(5);

enum Event {
	Link(Link),
	Properties(Vec<Property>),
	Message(Message),
	Applications(Vec<VrApplication>),
	Failed { request: &'static str, error: String },
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
	HeadsetStats(Option<HeadsetStats>),
}

pub struct LvrrLink {
	state: LvrrState,
	requests: Sender<LvrrRequest>,
	events: Receiver<Event>,
}

impl LvrrLink {
	pub fn start() -> anyhow::Result<Self> {
		let (requests, requests_rx) = mpsc::channel();
		let (events_tx, events) = mpsc::channel();
		thread::Builder::new()
			.name("lvrr-dbus".into())
			.spawn(move || worker(&requests_rx, &events_tx))?;
		Ok(Self {
			state: LvrrState::default(),
			requests,
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
		if self.state.link != Link::Online {
			anyhow::bail!("The LVRR server is not running");
		}
		self
			.requests
			.send(request)
			.map_err(|_| anyhow::anyhow!("The LVRR server link stopped"))
	}
}

fn apply(state: &mut LvrrState, event: Event, notices: &mut Vec<Notice>) {
	match event {
		Event::Link(link) => {
			state.link = link;
			if link != Link::Online {
				state.headset_connected = false;
				state.session_running = false;
				state.headset = None;
				state.stats.clear();
			}
			state.changed();
		}
		Event::Properties(properties) => {
			let mut changed = false;
			for property in properties {
				changed |= !matches!(property, Property::HeadsetStats(_));
				match property {
					Property::HeadsetConnected(v) => state.headset_connected = v,
					Property::SessionRunning(v) => state.session_running = v,
					Property::SystemName(v) => state.system_name = v,
					Property::Bitrate(v) => state.bitrate_bps = v,
					Property::PairingEnabled(v) => state.pairing_enabled = v,
					Property::EncryptionEnabled(v) => state.encryption_enabled = v,
					Property::Pin(v) => state.pin = v,
					Property::KnownKeys(v) => state.known_keys = v,
					Property::SteamCommand(v) => state.steam_command = v,
					Property::ServerConfig(v) => state.server_config = v,
					Property::HeadsetSettings(v) => state.headset = v,
					Property::HeadsetStats(Some(v)) => state.push_stats(v),
					Property::HeadsetStats(None) => {}
				}
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
			state.changed();
		}
		Event::Failed { request, error } => notices.push(Notice::RequestFailed { request, error }),
	}
}

fn worker(requests: &Receiver<LvrrRequest>, events: &Sender<Event>) {
	loop {
		match Connection::new_session() {
			Ok(connection) => match run(&connection, requests, events) {
				Ok(()) => return, // the dashboard is gone
				Err(e) => log::warn!("LVRR server link lost: {e:?}"),
			},
			Err(e) => log::warn!("No session bus for the LVRR server link: {e:?}"),
		}
		if events.send(Event::Link(Link::NoBus)).is_err() {
			return;
		}
		thread::sleep(RETRY_DELAY);
	}
}

fn run(connection: &Connection, requests: &Receiver<LvrrRequest>, events: &Sender<Event>) -> anyhow::Result<()> {
	// Signal callbacks can't call the server themselves, they leave it to the loop below
	let refresh = Arc::new(AtomicBool::new(true));

	connection.add_match(
		MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged")
			.with_sender(NAME)
			.with_path(PATH),
		{
			let events = events.clone();
			let refresh = refresh.clone();
			move |signal: PropertiesPropertiesChanged, _: &Connection, _: &dbus::Message| {
				if signal.interface_name == INTERFACE {
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

	connection.add_match(
		MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged").with_sender("org.freedesktop.DBus"),
		{
			let events = events.clone();
			let refresh = refresh.clone();
			move |(name, _old, new): (String, String, String), _: &Connection, _: &dbus::Message| {
				if name == NAME {
					if new.is_empty() {
						let _ = events.send(Event::Link(Link::ServerAbsent));
					} else {
						refresh.store(true, Ordering::Relaxed);
					}
				}
				true
			}
		},
	)?;

	loop {
		connection.process(Duration::from_millis(50))?;

		if refresh.swap(false, Ordering::Relaxed) {
			fetch_all(connection, events);
		}

		loop {
			match requests.try_recv() {
				Ok(request) => call(connection, request, events),
				Err(TryRecvError::Empty) => break,
				Err(TryRecvError::Disconnected) => return Ok(()),
			}
		}
	}
}

fn fetch_all(connection: &Connection, events: &Sender<Event>) {
	let proxy = connection.with_proxy(NAME, PATH, CALL_TIMEOUT);
	match proxy.get_all(INTERFACE) {
		Ok(properties) => {
			let _ = events.send(Event::Link(Link::Online));
			let _ = events.send(Event::Properties(decode(&properties)));
		}
		Err(e) => {
			log::debug!("LVRR server not reachable: {e}");
			let _ = events.send(Event::Link(Link::ServerAbsent));
		}
	}
}

fn call(connection: &Connection, request: LvrrRequest, events: &Sender<Event>) {
	let proxy = connection.with_proxy(NAME, PATH, CALL_TIMEOUT);
	let name = request.name();
	let result: Result<(), dbus::Error> = match request {
		LvrrRequest::SetHeadsetSettings(patch) => {
			proxy
				.method_call(INTERFACE, name, (patch.to_string(),))
				.map(|(settings,): (String,)| {
					let _ = events.send(Event::Properties(vec![Property::HeadsetSettings(parse(&settings))]));
				})
		}
		LvrrRequest::RestartSession | LvrrRequest::Disconnect | LvrrRequest::Quit | LvrrRequest::DisablePairing => {
			proxy.method_call(INTERFACE, name, ())
		}
		LvrrRequest::EnablePairing { timeout_secs } => proxy
			.method_call(INTERFACE, name, (timeout_secs,))
			.map(|(_pin,): (String,)| ()),
		LvrrRequest::RevokeKey(key) => proxy.method_call(INTERFACE, name, (key,)),
		LvrrRequest::RenameKey { key, name: new_name } => proxy.method_call(INTERFACE, name, (key, new_name)),
		LvrrRequest::SetServerConfig(config) => proxy.set(INTERFACE, "JsonConfiguration", config.to_string()),
		LvrrRequest::RefreshApplications => {
			proxy
				.method_call(INTERFACE, name, ())
				.map(|(applications,): (Vec<(String, String, String)>,)| {
					let applications = applications
						.into_iter()
						.map(|(id, name, icon)| VrApplication { id, name, icon })
						.collect();
					let _ = events.send(Event::Applications(applications));
				})
		}
		LvrrRequest::StartApplication(id) => proxy.method_call(INTERFACE, name, (id,)),
		LvrrRequest::StopApplication(id) => proxy.method_call(INTERFACE, name, (id,)),
		LvrrRequest::SetDashboardVisible(visible) => proxy.method_call(INTERFACE, name, (visible,)),
	};

	if let Err(e) = result {
		let _ = events.send(Event::Failed {
			request: name,
			error: e.message().unwrap_or("unknown error").to_string(),
		});
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
			"HeadsetStats" => string().map(|s| Property::HeadsetStats(parse(&s))),
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
