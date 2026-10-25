//! LVRR additions to the dashboard frame.

use std::time::Duration;

use lvrr_common::state::LvrrRequest;
use wgui::{
	components::{ComponentTrait, button::ComponentButton},
	event::StyleSetRequest,
	i18n::Translation,
	layout::LayoutTask,
	parser::Fetchable,
	taffy,
};
use wlx_common::config_io;

use super::{Frontend, FrontendTask};

/// The page open before a session restart
const REOPEN_FILE: &str = "lvrr-reopen";
/// Older saved pages are from an earlier session
const REOPEN_WITHIN: Duration = Duration::from_secs(120);
use crate::{
	tab::{TabType, lvrr::Page},
	views,
};

impl<T: 'static> Frontend<T> {
	pub(super) fn lvrr_register_widgets(&mut self) -> anyhow::Result<()> {
		for (id, page) in [
			("btn_side_library", Page::Library),
			("btn_side_running", Page::Running),
			("btn_side_headset", Page::Headset),
			("btn_side_pc", Page::Pc),
		] {
			let button = self.state.fetch_component_as::<ComponentButton>(id)?;
			self
				.tasks
				.handle_button(&button, FrontendTask::SetTab(TabType::Lvrr(page)));
			self.side_buttons.push((TabType::Lvrr(page), button));
		}
		if self.interface.lvrr_desktop() {
			self.lvrr_desktop_layout()?;
		} else {
			self.lvrr_reopen();
		}
		Ok(())
	}

	/// The desktop window has no VR pages, power menu or clock; its close button closes the window
	fn lvrr_desktop_layout(&mut self) -> anyhow::Result<()> {
		let mut hidden = vec![
			self.state.get_widget_id("side_separator")?,
			self.state.get_widget_id("bottom_bar")?,
		];
		for id in [
			"btn_side_library",
			"btn_side_apps",
			"btn_side_running",
			"btn_side_settings",
		] {
			hidden.push(self.state.fetch_component_as::<ComponentButton>(id)?.base().get_id());
		}
		for id in hidden {
			self.layout.tasks.push(LayoutTask::SetWidgetStyle(
				id,
				StyleSetRequest::Display(taffy::Display::None),
			));
		}
		Ok(())
	}

	/// Opens PC > Headsets and starts pairing, for the desktop window's tray
	pub fn lvrr_start_pairing(&mut self) {
		crate::tab::lvrr::pc::START_PAIRING.set(true);
		self.lvrr_open("pc:headsets");
	}

	/// The open page, for [`Self::lvrr_open`]
	fn lvrr_page(&self) -> Option<String> {
		Some(match self.current_tab.as_ref()?.get_type() {
			TabType::Home => "home".into(),
			TabType::Apps => "apps".into(),
			TabType::Settings => "settings".into(),
			TabType::Lvrr(Page::Library) => "library".into(),
			TabType::Lvrr(Page::Running) => "running".into(),
			TabType::Lvrr(Page::Headset) => format!("headset:{}", crate::tab::lvrr::section()),
			TabType::Lvrr(Page::Pc) => format!("pc:{}", crate::tab::lvrr::section()),
			_ => return None,
		})
	}

	/// A session restart restarts the dashboard too; it comes back on the same page
	fn lvrr_save_page(&self) {
		let Some(page) = self.lvrr_page() else {
			return;
		};
		if let Err(e) = std::fs::write(config_io::get_config_file_path(REOPEN_FILE), page) {
			log::warn!("Could not save the open page: {e}");
		}
	}

	fn lvrr_reopen(&mut self) {
		let path = config_io::get_config_file_path(REOPEN_FILE);
		let Ok(page) = std::fs::read_to_string(&path) else {
			return;
		};
		let recent = std::fs::metadata(&path)
			.and_then(|m| m.modified())
			.ok()
			.and_then(|t| t.elapsed().ok())
			.is_some_and(|age| age < REOPEN_WITHIN);
		let _ = std::fs::remove_file(&path);
		if recent {
			self.lvrr_open(page.trim());
		}
	}

	/// Opens a page by name, such as `headset:streaming`; for uidev
	pub fn lvrr_open(&mut self, page: &str) {
		let (name, section) = page.split_once(':').unwrap_or((page, ""));
		let tab = match name {
			"home" => TabType::Home,
			"apps" => TabType::Apps,
			"settings" => TabType::Settings,
			"library" => TabType::Lvrr(Page::Library),
			"running" => TabType::Lvrr(Page::Running),
			"headset" => TabType::Lvrr(Page::Headset),
			"pc" => TabType::Lvrr(Page::Pc),
			_ => return,
		};
		crate::tab::lvrr::headset::INITIAL_SECTION.set(crate::tab::lvrr::headset::section_name(section));
		crate::tab::lvrr::pc::INITIAL_SECTION.set(crate::tab::lvrr::pc::section_name(section));
		self.tasks.push(FrontendTask::SetTab(tab));
	}

	pub(super) fn lvrr_power_action(&mut self, action: &str) {
		match action {
			"lvrr_restart_session" => self.lvrr_confirm("LVRR.RESTART_SESSION_CONFIRM", LvrrRequest::RestartSession),
			"lvrr_disconnect" => self.tasks.push(FrontendTask::Lvrr(LvrrRequest::Disconnect)),
			"lvrr_quit" => self.lvrr_confirm("LVRR.QUIT_CONFIRM", LvrrRequest::Quit),
			_ => log::warn!("Unknown power action {action}"),
		}
	}

	pub(super) fn lvrr_request(&mut self, data: &mut T, request: LvrrRequest) {
		// the desktop window doesn't restart with the session
		if matches!(request, LvrrRequest::RestartSession) && !self.interface.lvrr_desktop() {
			self.lvrr_save_page();
		}
		let result = self.interface.lvrr_request(data, request);
		self.lvrr_toast_error(result);
	}

	pub(super) fn lvrr_pair(&mut self, data: &mut T) {
		let result = self.interface.lvrr_pair(data);
		self.lvrr_toast_error(result);
	}

	pub(super) fn lvrr_start_server(&mut self, data: &mut T) {
		let result = self.interface.lvrr_start_server(data);
		self.lvrr_toast_error(result);
	}

	fn lvrr_toast_error(&self, result: anyhow::Result<()>) {
		if let Err(e) = result {
			self
				.tasks
				.push(FrontendTask::PushToast(Translation::from_raw_text_string(
					e.to_string(),
				)));
		}
	}

	/// Sends `request` once the user confirms `message`
	pub(crate) fn lvrr_confirm(&self, message: &str, request: LvrrRequest) {
		self.lvrr_confirm_task(message, FrontendTask::Lvrr(request));
	}

	/// Runs `task` once the user confirms `message`
	pub(crate) fn lvrr_confirm_task(&self, message: &str, task: FrontendTask) {
		let tasks = self.tasks.clone();
		views::dialog_box::mount_popup(
			self.popup_dialog_box.clone(),
			self.tasks.clone(),
			views::dialog_box::Params {
				globals: self.globals.clone(),
				message: Translation::from_translation_key(message),
				entries: views::dialog_box::get_entries_yes_no(),
				on_action_click: Box::new(move |action| {
					if action == "yes" {
						tasks.push(task.clone());
					}
				}),
			},
		);
	}
}
