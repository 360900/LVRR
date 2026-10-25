//! LVRR additions to the dashboard frame.

use lvrr_common::state::LvrrRequest;
use wgui::{components::button::ComponentButton, i18n::Translation, parser::Fetchable};

use super::{Frontend, FrontendTask};
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
		Ok(())
	}

	/// Opens a page by name, such as `headset:streaming`; for uidev
	pub fn lvrr_open(&mut self, page: &str) {
		let (name, section) = page.split_once(':').unwrap_or((page, ""));
		let tab = match name {
			"home" => TabType::Home,
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
		if let Err(e) = self.interface.lvrr_request(data, request) {
			self
				.tasks
				.push(FrontendTask::PushToast(Translation::from_raw_text_string(
					e.to_string(),
				)));
		}
	}

	/// Sends `request` once the user confirms `message`
	pub(crate) fn lvrr_confirm(&self, message: &str, request: LvrrRequest) {
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
						tasks.push(FrontendTask::Lvrr(request));
					}
				}),
			},
		);
	}
}
