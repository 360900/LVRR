//! The LVRR home page: headset status and quick actions.

use std::marker::PhantomData;

use lvrr_common::state::{Link, LvrrRequest, MessageKind};
use wgui::{i18n::Translation, layout::WidgetID, parser::ParserState, task::Tasks};

use crate::{
	frontend::{Frontend, FrontendTask},
	tab::{
		Tab, TabType,
		lvrr::{
			Page,
			ui::{self, Text, Ui},
		},
	},
};

#[derive(Clone)]
enum Task {
	Frontend(FrontendTask),
	RestartSession,
}

/// Live values, updated in place with each statistics sample
struct Stats {
	refresh_rate: WidgetID,
	bitrate: WidgetID,
	latency: WidgetID,
	network: WidgetID,
}

pub struct TabLvrrHome<T> {
	#[allow(dead_code)]
	state: ParserState,
	widgets: Option<ParserState>,
	tasks: Tasks<Task>,
	parent: WidgetID,
	rendered: Option<u64>,
	stats_rendered: Option<u64>,
	stats: Option<Stats>,
	marker: PhantomData<T>,
}

impl<T: 'static> Tab<T> for TabLvrrHome<T> {
	fn get_type(&self) -> TabType {
		TabType::Home
	}

	fn update(&mut self, frontend: &mut Frontend<T>, _time_ms: u32, data: &mut T) -> anyhow::Result<()> {
		for task in self.tasks.drain() {
			match task {
				Task::Frontend(task) => frontend.tasks.push(task),
				Task::RestartSession => frontend.lvrr_confirm("LVRR.RESTART_SESSION_CONFIRM", LvrrRequest::RestartSession),
			}
		}

		let (generation, stats_generation) = frontend
			.interface
			.lvrr_state(data)
			.map_or((0, 0), |s| (s.generation, s.stats_generation));
		if self.rendered != Some(generation) {
			self.render(frontend, data, generation)?;
			self.stats_rendered = None;
		}
		if self.stats_rendered != Some(stats_generation) {
			self.stats_rendered = Some(stats_generation);
			self.update_stats(frontend, data);
		}
		Ok(())
	}
}

impl<T: 'static> TabLvrrHome<T> {
	pub fn new(frontend: &mut Frontend<T>, parent: WidgetID, _data: &mut T) -> anyhow::Result<Self> {
		let globals = frontend.layout.state.globals.clone();
		let state = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, parent)?;
		Ok(Self {
			state,
			widgets: None,
			tasks: Tasks::new(),
			parent,
			rendered: None,
			stats_rendered: None,
			stats: None,
			marker: PhantomData,
		})
	}

	fn render(&mut self, frontend: &mut Frontend<T>, data: &mut T, generation: u64) -> anyhow::Result<()> {
		self.rendered = Some(generation);
		let state = frontend.interface.lvrr_state(data).cloned().unwrap_or_default();

		frontend.layout.remove_children(self.parent);
		let globals = frontend.layout.state.globals.clone();
		let mut widgets = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, self.parent)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut widgets, &globals);
		let parent = self.parent;

		if state.link != Link::Online {
			ui.banner(parent, "LVRR.OFFLINE", "danger")?;
		}

		// The headset
		let card = ui.group(parent, "LVRR.HEADSET.TITLE", "@/dashboard/wivrn_head_symbolic.svg")?;
		let row = ui.row(card)?;
		if state.headset_connected {
			ui.bold(row, Text::Raw(&state.system_name), 20)?;
			ui.badge(row, "LVRR.HOME.CONNECTED", "success")?;
		} else {
			ui.bold(row, Text::Key("LVRR.HOME.NOT_CONNECTED"), 20)?;
		}
		self.stats = if state.headset_connected {
			let stats = ui.row(card)?;
			let ids = Stats {
				refresh_rate: ui.stat(stats, "LVRR.HOME.REFRESH_RATE")?,
				bitrate: ui.stat(stats, "LVRR.HOME.BITRATE")?,
				latency: ui.stat(stats, "LVRR.HOME.LATENCY")?,
				network: ui.stat(stats, "LVRR.HOME.NETWORK")?,
			};
			if let Some(headset) = &state.headset {
				let codec = ui.stat(stats, "LVRR.HOME.CODEC")?;
				ui::set_label(
					ui.layout,
					codec,
					Translation::from_raw_text(headset.config.codec.display_name()),
				);
				let [w, h] = headset.capabilities.eye_size;
				let scale = headset.config.resolution_scale;
				let resolution = ui.stat(stats, "LVRR.HOME.RESOLUTION")?;
				ui::set_label(
					ui.layout,
					resolution,
					Translation::from_raw_text_string(format!("{}×{}", (w as f32 * scale) as u32, (h as f32 * scale) as u32)),
				);
			}
			Some(ids)
		} else {
			None
		};

		if state.pairing_enabled && !state.pin.is_empty() {
			let banner = ui.banner(parent, "LVRR.HOME.PAIRING", "primary")?;
			ui.bold(banner, Text::Raw(&state.pin), 24)?;
		}
		if state.headset.as_ref().is_some_and(|h| h.restart_required) {
			let banner = ui.banner(parent, "LVRR.RESTART_REQUIRED", "tertiary")?;
			let tasks = self.tasks.clone();
			ui.button(
				banner,
				Text::Key("LVRR.RESTART_SESSION"),
				"@/dashboard/refresh.svg",
				move || {
					tasks.push(Task::RestartSession);
				},
			)?;
		}

		// Quick actions
		let group = ui.group(parent, "LVRR.HOME.QUICK_ACTIONS", "@/dashboard/magic_wand.svg")?;
		let row = ui.row(group)?;
		let actions = [
			(
				"LVRR.LIBRARY.TITLE",
				"@/dashboard/games.svg",
				FrontendTask::SetTab(TabType::Lvrr(Page::Library)),
			),
			(
				"APPLICATIONS",
				"@/dashboard/apps.svg",
				FrontendTask::SetTab(TabType::Apps),
			),
			(
				"LVRR.HEADSET.TITLE",
				"@/dashboard/wivrn_head_symbolic.svg",
				FrontendTask::SetTab(TabType::Lvrr(Page::Headset)),
			),
			(
				"LVRR.PC.TITLE",
				"@/dashboard/display.svg",
				FrontendTask::SetTab(TabType::Lvrr(Page::Pc)),
			),
			(
				"LVRR.HOME.RECENTER",
				"@/dashboard/recenter.svg",
				FrontendTask::RecenterPlayspace,
			),
			(
				"LVRR.DISCONNECT",
				"@/dashboard/close.svg",
				FrontendTask::Lvrr(LvrrRequest::Disconnect),
			),
		];
		for (translation, icon, task) in actions {
			let tasks = self.tasks.clone();
			ui.action(row, translation, icon, move || tasks.push(Task::Frontend(task.clone())))?;
		}

		// What the server told the headset, newest first
		if !state.messages.is_empty() {
			let group = ui.group(parent, "LVRR.HOME.MESSAGES", "@/dashboard/info.svg")?;
			for message in state.messages.iter().rev().take(5) {
				let row = ui.row(group)?;
				if message.kind == MessageKind::Error {
					ui.badge(row, "LVRR.HOME.ERROR", "danger")?;
				}
				ui.text(row, Text::Raw(&message.text))?;
			}
		}

		self.widgets = Some(widgets);
		Ok(())
	}

	fn update_stats(&mut self, frontend: &mut Frontend<T>, data: &mut T) {
		let Some(ids) = &self.stats else {
			return;
		};
		let Some(state) = frontend.interface.lvrr_state(data) else {
			return;
		};
		let bitrate = state.bitrate_bps;
		let stats = state.latest_stats().cloned();
		let layout = &mut frontend.layout;
		let text = Translation::from_raw_text_string;
		ui::set_label(layout, ids.bitrate, text(format!("{} Mbit/s", bitrate / 1_000_000)));
		if let Some(stats) = stats {
			ui::set_label(layout, ids.refresh_rate, text(format!("{:.0} Hz", stats.display_hz)));
			ui::set_label(layout, ids.latency, text(format!("{:.0} ms", stats.latency_ms())));
			ui::set_label(layout, ids.network, text(format!("{:.0} Mbit/s", stats.rx_bps / 1e6)));
		}
	}
}
