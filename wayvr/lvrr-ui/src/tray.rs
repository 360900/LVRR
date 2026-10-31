//! The tray icon: LVRR's status, and the window or LVRR's server a click away.

use ksni::{
	Category, MenuItem, ToolTip,
	blocking::{Handle, TrayMethods},
	menu::StandardItem,
};
use lvrr_common::state::Link;
use winit::event_loop::EventLoopProxy;

use crate::{UserEvent, instance::Request};

/// Menu labels, translated once
pub struct Labels {
	pub open: String,
	pub pair: String,
	pub start: String,
	pub stop: String,
	pub quit: String,
}

/// What runs meanwhile: a start of LVRR, from Start LVRR or Pair headset, or the tray's Stop LVRR
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
	#[default]
	Idle,
	/// `lvrr start`, until the link reaches LVRR or the start fails
	Starting,
	/// `lvrr stop`, which the app quits after unless the window opened meanwhile
	Stopping,
}

pub struct Tray {
	/// The server's state in words, empty until it's known
	pub status: String,
	/// The server's link, which picks Start or Stop LVRR
	pub link: Link,
	/// Leaves out the items that do nothing meanwhile
	pub phase: Phase,
	labels: Labels,
	/// The install's icons, which aren't in the icon theme
	icon_theme_path: String,
	proxy: EventLoopProxy<UserEvent>,
}

/// Shows the tray icon; None when the desktop has no tray
pub fn spawn(labels: Labels, proxy: EventLoopProxy<UserEvent>) -> Option<Handle<Tray>> {
	let icon_theme_path = lvrr_common::install_prefix()
		.map(|prefix| prefix.join("share/icons").display().to_string())
		.unwrap_or_default();
	let tray = Tray {
		status: String::new(),
		link: Link::default(),
		phase: Phase::default(),
		labels,
		icon_theme_path,
		proxy,
	};
	match tray.spawn() {
		Ok(handle) => Some(handle),
		Err(e) => {
			log::warn!("No tray icon: {e}");
			None
		}
	}
}

impl Tray {
	fn send(&self, event: UserEvent) {
		let _ = self.proxy.send_event(event);
	}

	/// Opens the window with the click's activation token, which raises it on Wayland. Plasma sends
	/// the token just before the click, so it's there right away; waiting would only hold up the
	/// thread that receives it.
	fn open(&self, request: Request) {
		let token = ksni::current_xdg_activation_token().unwrap_or_default();
		self.send(UserEvent::Open(request, token));
	}
}

impl ksni::Tray for Tray {
	fn id(&self) -> String {
		"io.github.lvrr".into()
	}

	fn title(&self) -> String {
		"LVRR".into()
	}

	fn category(&self) -> Category {
		Category::ApplicationStatus
	}

	fn icon_name(&self) -> String {
		"io.github.lvrr".into()
	}

	fn icon_theme_path(&self) -> String {
		self.icon_theme_path.clone()
	}

	fn tool_tip(&self) -> ToolTip {
		ToolTip {
			title: "LVRR".into(),
			description: self.status.clone(),
			..Default::default()
		}
	}

	fn activate(&mut self, _x: i32, _y: i32) {
		self.open(Request::Open);
	}

	fn menu(&self) -> Vec<MenuItem<Self>> {
		let mut menu = Vec::new();
		// the tooltip has it too, but GNOME's AppIndicator shows no tooltips
		if !self.status.is_empty() {
			menu.push(
				StandardItem {
					// dbusmenu takes a single `_` for an access key, and a headset's name can have one
					label: self.status.replace('_', "__"),
					enabled: false,
					..Default::default()
				}
				.into(),
			);
			menu.push(MenuItem::Separator);
		}
		menu.extend([
			item(&self.labels.open, |tray| tray.open(Request::Open)),
			// it starts LVRR first when it's stopped
			item(&self.labels.pair, |tray| tray.open(Request::Pair)),
		]);
		// the app ignores Start, Stop and Quit until `lvrr stop` is done
		if self.phase == Phase::Stopping {
			return menu;
		}
		menu.push(MenuItem::Separator);
		if self.link == Link::Online {
			menu.push(item(&self.labels.stop, |tray| tray.send(UserEvent::Stop)));
		} else {
			// `lvrr start` refuses while another WiVRn server runs, so Home offers no Start then
			// either, nor while LVRR starts
			if self.link != Link::Foreign && self.phase != Phase::Starting {
				menu.push(item(&self.labels.start, |tray| tray.send(UserEvent::Start)));
			}
			// with no LVRR server to stop, Stop LVRR would only quit
			menu.push(item(&self.labels.quit, |tray| tray.send(UserEvent::Quit)));
		}
		menu
	}
}

/// A menu item that runs `activate` when clicked
fn item(label: &str, activate: impl Fn(&mut Tray) + Send + 'static) -> MenuItem<Tray> {
	StandardItem {
		label: label.to_string(),
		activate: Box::new(activate),
		..Default::default()
	}
	.into()
}
