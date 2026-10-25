//! The tray icon: LVRR's status, and the window or LVRR's server a click away.

use ksni::{
	Category, MenuItem, ToolTip,
	blocking::{Handle, TrayMethods},
	menu::StandardItem,
};
use winit::event_loop::EventLoopProxy;

use crate::{UserEvent, instance::Request};

/// Menu labels, translated once
pub struct Labels {
	pub open: String,
	pub pair: String,
	pub stop: String,
}

pub struct Tray {
	pub status: String,
	labels: Labels,
	/// The install's icons, which aren't in the icon theme
	icon_theme_path: String,
	proxy: EventLoopProxy<UserEvent>,
}

/// Shows the tray icon; None when the desktop has no tray
pub fn spawn(labels: Labels, proxy: EventLoopProxy<UserEvent>) -> Option<Handle<Tray>> {
	let icon_theme_path = std::env::current_exe()
		.ok()
		.and_then(|exe| Some(exe.canonicalize().ok()?.parent()?.parent()?.join("share/icons")))
		.map(|path| path.display().to_string())
		.unwrap_or_default();
	let tray = Tray {
		status: String::new(),
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
		self.send(UserEvent::Open(Request::Open, String::new()));
	}

	fn menu(&self) -> Vec<MenuItem<Self>> {
		vec![
			StandardItem {
				label: self.labels.open.clone(),
				activate: Box::new(|tray: &mut Self| tray.send(UserEvent::Open(Request::Open, String::new()))),
				..Default::default()
			}
			.into(),
			StandardItem {
				label: self.labels.pair.clone(),
				activate: Box::new(|tray: &mut Self| tray.send(UserEvent::Open(Request::Pair, String::new()))),
				..Default::default()
			}
			.into(),
			MenuItem::Separator,
			StandardItem {
				label: self.labels.stop.clone(),
				activate: Box::new(|tray: &mut Self| tray.send(UserEvent::Stop)),
				..Default::default()
			}
			.into(),
		]
	}
}
