//! One window per session: a second `lvrr-ui` hands its request to the first one over D-Bus.

use std::time::Duration;

use dbus::{
	blocking::{Connection, stdintf::org_freedesktop_dbus::RequestNameReply},
	channel::MatchingReceiver,
	message::MatchRule,
};
use winit::event_loop::EventLoopProxy;

use crate::UserEvent;

pub const NAME: &str = "io.github.lvrr.Ui";
const PATH: &str = "/io/github/lvrr/Ui";
const INTERFACE: &str = "io.github.lvrr.Ui";

/// What a launch asks of the window
#[derive(Clone, Copy)]
pub enum Request {
	Open,
	Pair,
}

impl Request {
	const fn method(self) -> &'static str {
		match self {
			Request::Open => "Activate",
			Request::Pair => "Pair",
		}
	}
}

/// The session bus with `NAME` owned, or None when another instance owns it
pub fn claim() -> anyhow::Result<Option<Connection>> {
	let conn = Connection::new_session()?;
	let reply = conn.request_name(NAME, false, false, true)?;
	Ok((reply == RequestNameReply::PrimaryOwner).then_some(conn))
}

/// Passes `request` and the launch's activation token to the running instance
pub fn forward(request: Request, token: &str) -> anyhow::Result<()> {
	let conn = Connection::new_session()?;
	let proxy = conn.with_proxy(NAME, PATH, Duration::from_secs(5));
	proxy.method_call::<(), _, _, _>(INTERFACE, request.method(), (token,))?;
	Ok(())
}

/// Answers other instances, and `lvrr stop`, until the window quits
pub fn serve(conn: Connection, proxy: EventLoopProxy<UserEvent>) {
	conn.start_receive(
		MatchRule::new_method_call(),
		Box::new(move |message, conn| {
			if message.path().as_deref() != Some(PATH) || message.interface().as_deref() != Some(INTERFACE) {
				return true;
			}
			let token = message.read1::<String>().unwrap_or_default();
			let event = match message.member().as_deref() {
				Some("Activate") => UserEvent::Open(Request::Open, token),
				Some("Pair") => UserEvent::Open(Request::Pair, token),
				Some("Quit") => UserEvent::Quit,
				_ => return true,
			};
			let _ = proxy.send_event(event);
			let _ = conn.channel().send(message.method_return());
			true
		}),
	);
	std::thread::Builder::new()
		.name("lvrr-ui-dbus".into())
		.spawn(move || while conn.process(Duration::from_secs(60)).is_ok() {})
		.ok();
}
