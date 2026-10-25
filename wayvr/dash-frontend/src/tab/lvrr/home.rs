//! The LVRR home page: headset status, what is playing and quick actions.

use std::marker::PhantomData;

use lvrr_common::state::{Battery, BatteryDevice, Link, LvrrRequest, LvrrState, MessageKind};
use wgui::{color::WguiColorName, i18n::Translation, layout::WidgetID, parser::ParserState, task::Tasks};
use wlx_common::dash_interface::ConfigChangeKind;

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

/// Ticks between reads of the batteries and running applications
const REFRESH_TICKS: u32 = 120;

/// Below this, a battery shows in the danger color
const BATTERY_LOW: f32 = 0.3;

#[derive(Clone)]
enum Task {
	Frontend(FrontendTask),
	RestartSession,
	Stop(i64),
	TogglePassthrough,
	TogglePerfOverlay,
	/// Desktop window
	Pair,
	Quit,
}

/// Live values, updated in place with each statistics sample
struct Stats {
	refresh_rate: WidgetID,
	bitrate: WidgetID,
	latency: WidgetID,
	network: WidgetID,
}

/// What the page shows; it is built again when this changes
#[derive(Clone, PartialEq)]
struct Shown {
	generation: u64,
	batteries: Vec<BatteryDevice>,
	playing: Option<(i64, String)>,
	passthrough: Option<bool>,
	perf_overlay: bool,
}

pub struct TabLvrrHome<T> {
	#[allow(dead_code)]
	state: ParserState,
	widgets: Option<ParserState>,
	tasks: Tasks<Task>,
	parent: WidgetID,
	ticks: u32,
	batteries: Vec<Battery>,
	playing: Option<(i64, String)>,
	shown: Option<Shown>,
	stats_rendered: Option<u64>,
	stats: Option<Stats>,
	battery_labels: Vec<WidgetID>,
	marker: PhantomData<T>,
}

impl<T: 'static> Tab<T> for TabLvrrHome<T> {
	fn get_type(&self) -> TabType {
		TabType::Home
	}

	fn update(&mut self, frontend: &mut Frontend<T>, _time_ms: u32, data: &mut T) -> anyhow::Result<()> {
		let mut refresh = self.ticks.is_multiple_of(REFRESH_TICKS);
		self.ticks += 1;
		for task in self.tasks.drain() {
			match task {
				Task::Frontend(task) => frontend.tasks.push(task),
				Task::RestartSession => frontend.lvrr_confirm("LVRR.RESTART_SESSION_CONFIRM", LvrrRequest::RestartSession),
				Task::Stop(id) => {
					if let Ok(id) = u32::try_from(id) {
						frontend.lvrr_confirm("LVRR.RUNNING.STOP_CONFIRM", LvrrRequest::StopApplication(id));
					}
					refresh = true;
				}
				Task::TogglePassthrough => {
					let config = frontend.interface.general_config(data);
					config.use_passthrough = !config.use_passthrough;
					frontend
						.interface
						.config_changed(data, ConfigChangeKind::EnvironmentBlend);
				}
				Task::TogglePerfOverlay => {
					let visible = frontend.interface.lvrr_perf_overlay(data);
					frontend.interface.lvrr_set_perf_overlay(data, !visible);
				}
				Task::Pair => frontend.lvrr_start_pairing(),
				Task::Quit => frontend.lvrr_confirm("LVRR.QUIT_CONFIRM", LvrrRequest::Quit),
			}
		}

		if refresh {
			self.batteries = frontend.interface.lvrr_batteries(data);
			let clients = frontend.interface.monado_client_list(data, true).unwrap_or_default();
			self.playing = clients
				.iter()
				.find(|c| c.is_primary)
				.or_else(|| clients.first())
				.map(|c| (c.id, c.name.clone()));
		}

		let (generation, stats_generation) = frontend
			.interface
			.lvrr_state(data)
			.map_or((0, 0), |s| (s.generation, s.stats_generation));
		let shown = Shown {
			generation,
			batteries: self.batteries.iter().map(|b| b.device).collect(),
			playing: self.playing.clone(),
			passthrough: frontend
				.interface
				.get_feats(data)
				.passthru
				.then(|| frontend.interface.general_config(data).use_passthrough),
			perf_overlay: frontend.interface.lvrr_perf_overlay(data),
		};
		if self.shown.as_ref() != Some(&shown) {
			self.render(frontend, data, &shown)?;
			self.shown = Some(shown);
			self.stats_rendered = None;
			refresh = true;
		}
		if refresh {
			self.update_batteries(frontend);
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
			ticks: 0,
			batteries: Vec::new(),
			playing: None,
			shown: None,
			stats_rendered: None,
			stats: None,
			battery_labels: Vec::new(),
			marker: PhantomData,
		})
	}

	fn render(&mut self, frontend: &mut Frontend<T>, data: &mut T, shown: &Shown) -> anyhow::Result<()> {
		let state = frontend.interface.lvrr_state(data).cloned().unwrap_or_default();

		frontend.layout.remove_children(self.parent);
		let globals = frontend.layout.state.globals.clone();
		let mut widgets = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, self.parent)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut widgets, &globals);
		let parent = self.parent;
		let desktop = frontend.interface.lvrr_desktop();

		if state.link != Link::Online && desktop {
			let banner = ui.banner(parent, "LVRR.DESKTOP.NOT_RUNNING", "danger")?;
			let tasks = self.tasks.clone();
			ui.button(
				banner,
				Text::Key("LVRR.DESKTOP.START"),
				"@/dashboard/play.svg",
				move || {
					tasks.push(Task::Frontend(FrontendTask::LvrrStartServer));
				},
			)?;
		} else if state.link != Link::Online {
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
		self.battery_labels.clear();
		if !shown.batteries.is_empty() {
			let row = ui.row(card)?;
			for device in &shown.batteries {
				self.battery_labels.push(ui.stat(row, device.key())?);
			}
		}

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

		// The application in front
		if let Some((id, name)) = &shown.playing {
			let group = ui.group(parent, "LVRR.HOME.NOW_PLAYING", "@/dashboard/play.svg")?;
			let row = ui.row(group)?;
			ui.bold(row, Text::Raw(name), 18)?;
			let tasks = self.tasks.clone();
			ui.button(row, Text::Key("LVRR.HOME.SWITCH"), "@/dashboard/apps.svg", move || {
				tasks.push(Task::Frontend(FrontendTask::SetTab(TabType::Lvrr(Page::Running))));
			})?;
			let (tasks, id) = (self.tasks.clone(), *id);
			ui.danger_button(
				row,
				Text::Key("LVRR.RUNNING.STOP"),
				"@/dashboard/close.svg",
				move || {
					tasks.push(Task::Stop(id));
				},
			)?;
		}

		if desktop {
			self.desktop_actions(&mut ui, &state)?;
			self.messages(&mut ui, &state)?;
			self.widgets = Some(widgets);
			return Ok(());
		}

		// Quick actions
		let group = ui.group(parent, "LVRR.HOME.QUICK_ACTIONS", "@/dashboard/magic_wand.svg")?;
		let row = ui.row(group)?;
		let actions = [
			(
				"LVRR.LIBRARY.TITLE",
				"@/dashboard/games.svg",
				Task::Frontend(FrontendTask::SetTab(TabType::Lvrr(Page::Library))),
			),
			(
				"APPLICATIONS",
				"@/dashboard/apps.svg",
				Task::Frontend(FrontendTask::SetTab(TabType::Apps)),
			),
			(
				"LVRR.HEADSET.TITLE",
				"@/dashboard/wivrn_head_symbolic.svg",
				Task::Frontend(FrontendTask::SetTab(TabType::Lvrr(Page::Headset))),
			),
			(
				"LVRR.PC.TITLE",
				"@/dashboard/display.svg",
				Task::Frontend(FrontendTask::SetTab(TabType::Lvrr(Page::Pc))),
			),
			(
				"LVRR.HOME.RECENTER",
				"@/dashboard/recenter.svg",
				Task::Frontend(FrontendTask::RecenterPlayspace),
			),
		];
		for (translation, icon, task) in actions {
			let tasks = self.tasks.clone();
			ui.action(row, translation, icon, move || tasks.push(task.clone()))?;
		}
		// toggles are highlighted while on
		let mut toggles = vec![(
			"LVRR.HOME.PERF_OVERLAY",
			"@/dashboard/barchart.svg",
			Task::TogglePerfOverlay,
			shown.perf_overlay,
		)];
		if let Some(on) = shown.passthrough {
			toggles.push((
				"LVRR.HOME.PASSTHROUGH",
				"@/dashboard/eye.svg",
				Task::TogglePassthrough,
				on,
			));
		}
		for (translation, icon, task, on) in toggles {
			let tasks = self.tasks.clone();
			let button = ui.action(row, translation, icon, move || tasks.push(task.clone()))?;
			let mut common = ui.layout.common();
			button.set_sticky_state(&mut common, on);
		}
		let tasks = self.tasks.clone();
		ui.action(row, "LVRR.DISCONNECT", "@/dashboard/close.svg", move || {
			tasks.push(Task::Frontend(FrontendTask::Lvrr(LvrrRequest::Disconnect)));
		})?;

		self.messages(&mut ui, &state)?;
		self.widgets = Some(widgets);
		Ok(())
	}

	/// What the server told the headset, newest first
	fn messages(&self, ui: &mut Ui, state: &LvrrState) -> anyhow::Result<()> {
		if !state.messages.is_empty() {
			let group = ui.group(self.parent, "LVRR.HOME.MESSAGES", "@/dashboard/info.svg")?;
			for message in state.messages.iter().rev().take(5) {
				let row = ui.row(group)?;
				if message.kind == MessageKind::Error {
					ui.badge(row, "LVRR.HOME.ERROR", "danger")?;
				}
				ui.text(row, Text::Raw(&message.text))?;
			}
		}
		Ok(())
	}

	/// The desktop window's actions: no VR apps, overlays or playspace there
	fn desktop_actions(&self, ui: &mut Ui, state: &LvrrState) -> anyhow::Result<()> {
		if state.link != Link::Online {
			return Ok(());
		}
		let group = ui.group(self.parent, "LVRR.HOME.QUICK_ACTIONS", "@/dashboard/magic_wand.svg")?;
		let row = ui.row(group)?;
		let mut actions = vec![
			(
				"LVRR.HEADSET.TITLE",
				"@/dashboard/wivrn_head_symbolic.svg",
				Task::Frontend(FrontendTask::SetTab(TabType::Lvrr(Page::Headset))),
			),
			(
				"LVRR.PC.TITLE",
				"@/dashboard/display.svg",
				Task::Frontend(FrontendTask::SetTab(TabType::Lvrr(Page::Pc))),
			),
			("LVRR.PC.PAIR", "@/dashboard/add.svg", Task::Pair),
		];
		if state.headset_connected {
			actions.push((
				"LVRR.DISCONNECT",
				"@/dashboard/close.svg",
				Task::Frontend(FrontendTask::Lvrr(LvrrRequest::Disconnect)),
			));
		}
		actions.push(("LVRR.QUIT", "@/dashboard/power.svg", Task::Quit));
		for (translation, icon, task) in actions {
			let tasks = self.tasks.clone();
			ui.action(row, translation, icon, move || tasks.push(task.clone()))?;
		}
		Ok(())
	}

	fn update_batteries(&self, frontend: &mut Frontend<T>) {
		for (battery, &label) in self.batteries.iter().zip(&self.battery_labels) {
			let layout = &mut frontend.layout;
			let text = format!("{:.0}%", battery.level * 100.0);
			ui::set_label(layout, label, Translation::from_raw_text_string(text));
			let color = if battery.charging {
				WguiColorName::Primary
			} else if battery.level < BATTERY_LOW {
				WguiColorName::Danger
			} else {
				WguiColorName::OnBackground
			};
			ui::set_label_color(layout, label, color);
		}
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
