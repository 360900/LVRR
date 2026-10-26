//! The PC side of LVRR: performance, paired headsets, server settings, system checks.
//! This replaces WiVRn's desktop dashboard.

use std::{
	marker::PhantomData,
	rc::Rc,
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use lvrr_common::{
	doctor::{Report, Status},
	headset::HeadsetStats,
	server_config::{ENCODERS, Encoder, OpenVrCompat, ServerConfig},
	state::{LvrrRequest, LvrrState},
};
use serde_json::{Value, json};
use wgui::{
	assets::AssetPathRef,
	components::{
		bar_graph::{ComponentBarGraph, ValueCell},
		editbox::ComponentEditBox,
		tabs::ComponentTabs,
	},
	drawing::Color,
	i18n::Translation,
	layout::WidgetID,
	parser::{Fetchable, ParseDocumentParams, ParserState},
	task::Tasks,
	windowing::context_menu::{self, Blueprint, ContextMenu, TickResult},
};

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

const DEBOUNCE: Duration = Duration::from_millis(400);
const ACTION_PREFIX: &str = "lvrr_pc;";

/// Headset names to pick from in VR, where there is no keyboard
const HEADSET_NAMES: &[&str] = &[
	"Quest 3",
	"Quest 3S",
	"Quest Pro",
	"Quest 2",
	"Pico 4",
	"Pico 4 Ultra",
	"Vive Focus 3",
	"Vive XR Elite",
	"Lynx R-1",
	"Galaxy XR",
];

thread_local! {
	/// Section to open first, for uidev (`LVRR_PAGE=pc:server`)
	pub(crate) static INITIAL_SECTION: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
	/// Start pairing once the page opens, for the desktop window's tray
	pub(crate) static START_PAIRING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const SECTIONS: [&str; 5] = ["performance", "headsets", "server", "diagnostics", "about"];

pub(crate) fn section_name(name: &str) -> Option<&'static str> {
	SECTIONS.into_iter().find(|n| *n == name)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Section {
	Performance,
	Headsets,
	Server,
	Diagnostics,
	About,
}

impl Section {
	const fn name(self) -> &'static str {
		match self {
			Section::Performance => "performance",
			Section::Headsets => "headsets",
			Section::Server => "server",
			Section::Diagnostics => "diagnostics",
			Section::About => "about",
		}
	}

	fn from_name(name: &str) -> Option<Self> {
		Some(match name {
			"performance" => Section::Performance,
			"headsets" => Section::Headsets,
			"server" => Section::Server,
			"diagnostics" => Section::Diagnostics,
			"about" => Section::About,
			_ => return None,
		})
	}
}

/// A change to the server configuration, applied to the latest one when sent
#[derive(Clone, Debug)]
enum ServerChange {
	Encoder(Encoder),
	Bool(&'static str, bool),
	Number(&'static str, Option<f64>),
	OpenVr(OpenVrCompat),
}

#[derive(Clone)]
enum Task {
	SetSection(Section),
	Confirm(&'static str, LvrrRequest),
	Server {
		change: ServerChange,
		now: bool,
	},
	OpenMenu(context_menu::Position, Vec<context_menu::Cell>),
	PerfOverlay(bool),
	Copy(String),
	RunChecks,
	Frontend(FrontendTask),
	/// Desktop window: pair a new headset, ending the session first if the user agrees
	Pair,
	/// Desktop window: a paired headset's public key and the field with its new name
	Rename(String, Rc<ComponentEditBox>),
}

struct Graphs {
	cpu: Rc<ComponentBarGraph>,
	gpu: Rc<ComponentBarGraph>,
	network: Rc<ComponentBarGraph>,
	latency: Rc<ComponentBarGraph>,
	motion_to_photon: Rc<ComponentBarGraph>,
}

type ChecksResult = Arc<Mutex<Option<Result<Report, String>>>>;

pub struct TabLvrrPc<T> {
	state: ParserState,
	widgets: Option<ParserState>,
	tasks: Tasks<Task>,
	id_content: WidgetID,
	section: Section,
	rendered: Option<u64>,
	stats_rendered: Option<u64>,
	graphs: Option<Graphs>,
	pending: Vec<ServerChange>,
	last_edit: Instant,
	context_menu: ContextMenu,
	checks: Option<Result<Report, String>>,
	checks_running: Option<ChecksResult>,
	version: Option<String>,
	marker: PhantomData<T>,
}

impl<T: 'static> Tab<T> for TabLvrrPc<T> {
	fn get_type(&self) -> TabType {
		TabType::Lvrr(Page::Pc)
	}

	fn update(&mut self, frontend: &mut Frontend<T>, _time_ms: u32, data: &mut T) -> anyhow::Result<()> {
		for task in self.tasks.drain() {
			match task {
				Task::SetSection(section) => {
					self.section = section;
					super::set_section(section.name());
					self.rendered = None;
				}
				Task::Confirm(message, request) => frontend.lvrr_confirm(message, request),
				Task::Server { change, now } => {
					self.pending.push(change);
					self.last_edit = Instant::now();
					if now {
						self.send_server(frontend, data);
					}
				}
				Task::OpenMenu(position, cells) => self.context_menu.open(context_menu::OpenParams {
					on_custom_attribs: None,
					position,
					blueprint: Blueprint::Cells(cells),
				}),
				Task::PerfOverlay(visible) => frontend.interface.lvrr_set_perf_overlay(data, visible),
				Task::Copy(text) => {
					let message = match frontend.interface.clipboard_set_text(data, &text) {
						Ok(()) => Translation::from_translation_key("LVRR.PC.COPIED"),
						Err(e) => Translation::from_raw_text_string(e.to_string()),
					};
					frontend.tasks.push(FrontendTask::PushToast(message));
				}
				Task::RunChecks => self.run_checks(),
				Task::Frontend(task) => frontend.tasks.push(task),
				Task::Pair => {
					let busy = frontend
						.interface
						.lvrr_state(data)
						.is_some_and(|s| s.session_running || s.headset_connected);
					if busy {
						frontend.lvrr_confirm_task("LVRR.PC.PAIR_CONFIRM", FrontendTask::LvrrPair);
					} else {
						frontend.tasks.push(FrontendTask::LvrrPair);
					}
				}
				Task::Rename(key, field) => {
					let name = field.get_text().trim().to_string();
					if !name.is_empty() {
						frontend
							.tasks
							.push(FrontendTask::Lvrr(LvrrRequest::RenameKey { key, name }));
					}
				}
			}
		}

		if !self.pending.is_empty() && self.last_edit.elapsed() >= DEBOUNCE {
			self.send_server(frontend, data);
		}

		if let TickResult::Action(name) = self.context_menu.tick(&mut frontend.layout, &mut self.state)? {
			match parse_action(&name) {
				Some(MenuAction::Server(change)) => self.tasks.push(Task::Server { change, now: true }),
				Some(MenuAction::Request(request)) => frontend.tasks.push(FrontendTask::Lvrr(request)),
				None => {}
			}
		}

		let finished = self.checks_running.as_ref().and_then(|r| r.lock().unwrap().take());
		if let Some(result) = finished {
			self.checks = Some(result);
			self.checks_running = None;
			self.rendered = None;
		}

		let (generation, stats_generation) = frontend
			.interface
			.lvrr_state(data)
			.map_or((0, 0), |s| (s.generation, s.stats_generation));
		if self.pending.is_empty() && self.rendered != Some(generation) {
			self.render(frontend, data, generation)?;
			self.stats_rendered = Some(stats_generation);
		}
		if self.stats_rendered != Some(stats_generation) {
			self.stats_rendered = Some(stats_generation);
			if let (Some(graphs), Some(state)) = (&self.graphs, frontend.interface.lvrr_state(data))
				&& let Some(stats) = state.latest_stats()
			{
				push_stats(graphs, stats);
				frontend.layout.mark_redraw();
			}
		}
		Ok(())
	}
}

enum MenuAction {
	Server(ServerChange),
	Request(LvrrRequest),
}

fn parse_action(name: &str) -> Option<MenuAction> {
	let (kind, value) = name.strip_prefix(ACTION_PREFIX)?.split_once(';')?;
	Some(match kind {
		"encoder" if value == "auto" => MenuAction::Server(ServerChange::Encoder(Encoder::Automatic)),
		"encoder" => MenuAction::Server(ServerChange::Encoder(Encoder::Named(value.to_string()))),
		"openvr" => MenuAction::Server(ServerChange::OpenVr(match value {
			"unmanaged" => OpenVrCompat::Unmanaged,
			_ => OpenVrCompat::Automatic,
		})),
		// names have no ';', keys might
		"rename" => {
			let (name, key) = value.split_once(';')?;
			MenuAction::Request(LvrrRequest::RenameKey {
				key: key.to_string(),
				name: name.to_string(),
			})
		}
		_ => return None,
	})
}

/// Green below `warn`, red above
fn cell(value: f32, warn: f32) -> ValueCell {
	let color = if value > warn {
		Color::new(1.0, 0.35, 0.35, 1.0)
	} else {
		Color::new(0.3, 0.9, 0.5, 1.0)
	};
	ValueCell { value, color }
}

fn push_stats(graphs: &Graphs, stats: &HeadsetStats) {
	let budget = if stats.display_hz > 0.0 {
		1000.0 / stats.display_hz
	} else {
		11.1
	};
	graphs.cpu.push_value(cell(stats.cpu_time_ms, budget));
	graphs.gpu.push_value(cell(stats.gpu_time_ms, budget));
	graphs.network.push_value(cell(stats.rx_bps / 1e6, f32::MAX));
	graphs.latency.push_value(cell(stats.latency_ms(), 50.0));
	graphs
		.motion_to_photon
		.push_value(cell(stats.motion_to_photon_ms, 60.0));
}

fn format_time(seconds: i64) -> String {
	chrono::DateTime::from_timestamp(seconds, 0).map_or_else(String::new, |t| {
		t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()
	})
}

impl<T: 'static> TabLvrrPc<T> {
	pub fn new(frontend: &mut Frontend<T>, parent: WidgetID, _data: &mut T) -> anyhow::Result<Self> {
		let globals = frontend.layout.state.globals.clone();
		let state = wgui::parser::parse_from_assets(
			&ParseDocumentParams {
				globals,
				path: AssetPathRef::BuiltIn("gui/lvrr/pc.xml"),
				extra: Default::default(),
			},
			&mut frontend.layout,
			parent,
		)?;
		let id_content = state.get_widget_id("content")?;
		let tasks = Tasks::new();
		let section = INITIAL_SECTION
			.take()
			.and_then(Section::from_name)
			.unwrap_or(Section::Performance);
		super::set_section(section.name());
		if START_PAIRING.take() {
			tasks.push(Task::Pair);
		}

		let tabs = state.fetch_component_as::<ComponentTabs>("tabs")?;
		if section != Section::Performance {
			let mut common = frontend.layout.common();
			for name in SECTIONS {
				if let Some(button) = tabs.get_tab_button(name) {
					button.set_sticky_state(&mut common, Section::from_name(name) == Some(section));
				}
			}
		}
		tabs.on_select({
			let tasks = tasks.clone();
			Rc::new(move |_common, e| {
				if let Some(section) = Section::from_name(&e.name) {
					tasks.push(Task::SetSection(section));
				}
				Ok(())
			})
		});

		Ok(Self {
			state,
			widgets: None,
			tasks,
			id_content,
			section,
			rendered: None,
			stats_rendered: None,
			graphs: None,
			pending: Vec::new(),
			last_edit: Instant::now(),
			context_menu: ContextMenu::default(),
			checks: None,
			checks_running: None,
			version: None,
			marker: PhantomData,
		})
	}

	fn send_server(&mut self, frontend: &mut Frontend<T>, data: &mut T) {
		let Some(state) = frontend.interface.lvrr_state(data) else {
			self.pending.clear();
			return;
		};
		let mut config = ServerConfig(state.server_config.clone());
		for change in self.pending.drain(..) {
			match change {
				ServerChange::Encoder(encoder) => config.set_encoder(&encoder),
				ServerChange::Bool(key, value) => config.set(key, value.then_some(Value::Bool(true))),
				ServerChange::Number(key, value) => config.set(key, value.map(|v| json!(v))),
				ServerChange::OpenVr(compat) => config.set_openvr_compat(&compat),
			}
		}
		frontend
			.tasks
			.push(FrontendTask::Lvrr(LvrrRequest::SetServerConfig(config.0)));
	}

	fn run_checks(&mut self) {
		if self.checks_running.is_some() {
			return;
		}
		let result: ChecksResult = Arc::default();
		let out = result.clone();
		std::thread::spawn(move || {
			let report = Report::run().map_err(|e| e.to_string());
			*out.lock().unwrap() = Some(report);
		});
		self.checks_running = Some(result);
	}

	fn render(&mut self, frontend: &mut Frontend<T>, data: &mut T, generation: u64) -> anyhow::Result<()> {
		self.rendered = Some(generation);
		self.graphs = None;
		let state = frontend.interface.lvrr_state(data).cloned().unwrap_or_default();
		// the overlay is drawn in VR, the desktop window can't show it
		let perf_overlay = (!frontend.interface.lvrr_desktop()).then(|| frontend.interface.lvrr_perf_overlay(data));
		let desktop = frontend.interface.lvrr_desktop();

		frontend.layout.remove_children(self.id_content);
		let globals = frontend.layout.state.globals.clone();
		let mut widgets =
			wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, self.id_content)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut widgets, &globals);

		match self.section {
			Section::Performance => self.graphs = self.performance(&mut ui, &state, perf_overlay)?,
			Section::Headsets if desktop => self.headsets_desktop(&mut ui, &state)?,
			Section::Headsets => self.headsets(&mut ui, &state)?,
			Section::Server => self.server(&mut ui, &state)?,
			Section::Diagnostics => self.diagnostics(&mut ui)?,
			Section::About => self.about(&mut ui)?,
		}

		self.widgets = Some(widgets);
		Ok(())
	}

	fn performance(&self, ui: &mut Ui, state: &LvrrState, perf_overlay: Option<bool>) -> anyhow::Result<Option<Graphs>> {
		let group = ui.group(self.id_content, "LVRR.PC.PERFORMANCE", "@/dashboard/barchart.svg")?;
		if let Some(perf_overlay) = perf_overlay {
			let tasks = self.tasks.clone();
			ui.toggle(group, Text::Key("LVRR.PC.PERF_OVERLAY"), perf_overlay, move |v| {
				tasks.push(Task::PerfOverlay(v));
			})?;
		}
		if state.stats.is_empty() {
			ui.muted(group, Text::Key("LVRR.PC.NO_STATS"))?;
			return Ok(None);
		}
		let row = ui.row(group)?;
		let network_max = (state.bitrate_bps as f32 / 1e6 * 1.5).max(10.0);
		let graphs = Graphs {
			cpu: ui.graph(row, "LVRR.PC.CPU_TIME", "ms", 20.0)?,
			gpu: ui.graph(row, "LVRR.PC.GPU_TIME", "ms", 20.0)?,
			network: ui.graph(row, "LVRR.PC.NETWORK", "Mbit/s", network_max)?,
			latency: ui.graph(row, "LVRR.PC.LATENCY", "ms", 100.0)?,
			motion_to_photon: ui.graph(row, "LVRR.PC.MOTION_TO_PHOTON", "ms", 100.0)?,
		};
		for stats in &state.stats {
			push_stats(&graphs, stats);
		}
		Ok(Some(graphs))
	}

	fn headsets(&self, ui: &mut Ui, state: &LvrrState) -> anyhow::Result<()> {
		if state.pairing_enabled && !state.pin.is_empty() {
			let banner = ui.banner(self.id_content, "LVRR.HOME.PAIRING", "primary")?;
			ui.bold(banner, Text::Raw(&state.pin), 24)?;
		}
		let group = ui.group(
			self.id_content,
			"LVRR.PC.PAIRED_HEADSETS",
			"@/dashboard/wivrn_head_symbolic.svg",
		)?;
		if state.known_keys.is_empty() {
			ui.muted(group, Text::Key("LVRR.PC.NO_HEADSETS"))?;
		}
		for key in &state.known_keys {
			let row = ui.line(group)?;
			let column = ui.column(row)?;
			ui.bold(column, Text::Raw(&key.name), 16)?;
			match key.last_connection {
				Some(t) => ui.muted(column, Text::Raw(&format_time(t)))?,
				None => ui.muted(column, Text::Key("LVRR.PC.NEVER_CONNECTED"))?,
			}
			let cells = HEADSET_NAMES
				.iter()
				.map(|name| {
					ui::menu_cell(
						format!("{ACTION_PREFIX}rename;{name};{}", key.public_key),
						Translation::from_raw_text(name),
					)
				})
				.collect();
			let tasks = self.tasks.clone();
			ui.dropdown(
				row,
				"LVRR.PC.RENAME",
				Translation::from_raw_text(&key.name),
				cells,
				move |position, cells| {
					tasks.push(Task::OpenMenu(position, cells));
				},
			)?;
			let (tasks, public_key) = (self.tasks.clone(), key.public_key.clone());
			ui.danger_button(row, Text::Key("LVRR.PC.REVOKE"), "@/dashboard/trash.svg", move || {
				tasks.push(Task::Confirm(
					"LVRR.PC.REVOKE_CONFIRM",
					LvrrRequest::RevokeKey(public_key.clone()),
				));
			})?;
		}
		ui.muted(group, Text::Key("LVRR.PC.PAIR_HOWTO"))?;
		Ok(())
	}

	/// Pairing with a keyboard and mouse: a PIN on demand and names typed in
	fn headsets_desktop(&self, ui: &mut Ui, state: &LvrrState) -> anyhow::Result<()> {
		if state.pairing_enabled && !state.pin.is_empty() {
			let banner = ui.banner(self.id_content, "LVRR.PC.PAIR_PIN", "primary")?;
			ui.bold(banner, Text::Raw(&state.pin), 32)?;
			let tasks = self.tasks.clone();
			ui.button(
				banner,
				Text::Key("LVRR.PC.PAIR_STOP"),
				"@/dashboard/close.svg",
				move || {
					tasks.push(Task::Frontend(FrontendTask::Lvrr(LvrrRequest::DisablePairing)));
				},
			)?;
		} else {
			let row = ui.row(self.id_content)?;
			let tasks = self.tasks.clone();
			ui.button(row, Text::Key("LVRR.PC.PAIR"), "@/dashboard/add.svg", move || {
				tasks.push(Task::Pair);
			})?;
			ui.muted(row, Text::Key("LVRR.PC.PAIR_HOWTO_DESKTOP"))?;
		}
		let group = ui.group(
			self.id_content,
			"LVRR.PC.PAIRED_HEADSETS",
			"@/dashboard/wivrn_head_symbolic.svg",
		)?;
		if state.known_keys.is_empty() {
			ui.muted(group, Text::Key("LVRR.PC.NO_HEADSETS"))?;
		}
		for key in &state.known_keys {
			let row = ui.line(group)?;
			let column = ui.column(row)?;
			let field = ui.editbox(column, &key.name)?;
			match key.last_connection {
				Some(t) => ui.muted(column, Text::Raw(&format_time(t)))?,
				None => ui.muted(column, Text::Key("LVRR.PC.NEVER_CONNECTED"))?,
			}
			let (tasks, public_key) = (self.tasks.clone(), key.public_key.clone());
			ui.button(row, Text::Key("LVRR.PC.RENAME"), "@/dashboard/check.svg", move || {
				tasks.push(Task::Rename(public_key.clone(), field.clone()));
			})?;
			let (tasks, public_key) = (self.tasks.clone(), key.public_key.clone());
			ui.danger_button(row, Text::Key("LVRR.PC.REVOKE"), "@/dashboard/trash.svg", move || {
				tasks.push(Task::Confirm(
					"LVRR.PC.REVOKE_CONFIRM",
					LvrrRequest::RevokeKey(public_key.clone()),
				));
			})?;
		}
		Ok(())
	}

	fn server(&self, ui: &mut Ui, state: &LvrrState) -> anyhow::Result<()> {
		let config = ServerConfig(state.server_config.clone());
		let group = ui.group(self.id_content, "LVRR.PC.SERVER", "@/dashboard/cpu.svg")?;

		// encoder
		let row = ui.row(group)?;
		let current = match config.encoder() {
			Encoder::Automatic => Translation::from_translation_key("LVRR.HEADSET.AUTOMATIC"),
			Encoder::Named(name) => Translation::from_raw_text_string(name),
			Encoder::Custom => Translation::from_translation_key("LVRR.PC.ENCODER_CUSTOM"),
		};
		let mut cells = vec![ui::menu_cell(
			format!("{ACTION_PREFIX}encoder;auto"),
			Translation::from_translation_key("LVRR.HEADSET.AUTOMATIC"),
		)];
		cells.extend(
			ENCODERS
				.iter()
				.map(|e| ui::menu_cell(format!("{ACTION_PREFIX}encoder;{e}"), Translation::from_raw_text(e))),
		);
		let tasks = self.tasks.clone();
		ui.dropdown(row, "LVRR.PC.ENCODER", current, cells, move |position, cells| {
			tasks.push(Task::OpenMenu(position, cells));
		})?;

		// toggles
		for (key, label) in [
			("hid-forwarding", "LVRR.PC.HID_FORWARDING"),
			("use-steamvr-lh", "LVRR.PC.STEAMVR_LH"),
		] {
			let tasks = self.tasks.clone();
			ui.toggle(group, Text::Key(label), config.bool(key), move |v| {
				tasks.push(Task::Server {
					change: ServerChange::Bool(key, v),
					now: true,
				});
			})?;
		}

		// Lighthouse tuning
		if config.bool("use-steamvr-lh") {
			let tasks = self.tasks.clone();
			ui.slider(
				group,
				"LVRR.PC.LH_EXTRAPOLATION",
				None,
				(0.0, 100.0, 1.0),
				config.number("lh-max-extrapolation").unwrap_or(0.0) as f32,
				move |v| {
					tasks.push(Task::Server {
						change: ServerChange::Number("lh-max-extrapolation", (v > 0.0).then_some(f64::from(v))),
						now: false,
					});
				},
			)?;
			let tasks = self.tasks.clone();
			ui.slider(
				group,
				"LVRR.PC.LH_DEADZONE",
				None,
				(0.0, 0.5, 0.01),
				config.number("lh-stick-deadzone").unwrap_or(0.0) as f32,
				move |v| {
					tasks.push(Task::Server {
						change: ServerChange::Number("lh-stick-deadzone", (v > 0.0).then_some(f64::from(v))),
						now: false,
					});
				},
			)?;
		}

		// OpenVR
		let row = ui.row(group)?;
		let current = match config.openvr_compat() {
			OpenVrCompat::Automatic => "LVRR.PC.OPENVR_BUNDLED",
			OpenVrCompat::Path(_) => "LVRR.PC.OPENVR_CUSTOM",
			OpenVrCompat::Unmanaged => "LVRR.PC.OPENVR_UNMANAGED",
		};
		let cells = vec![
			ui::menu_cell(
				format!("{ACTION_PREFIX}openvr;bundled"),
				Translation::from_translation_key("LVRR.PC.OPENVR_BUNDLED"),
			),
			ui::menu_cell(
				format!("{ACTION_PREFIX}openvr;unmanaged"),
				Translation::from_translation_key("LVRR.PC.OPENVR_UNMANAGED"),
			),
		];
		let tasks = self.tasks.clone();
		ui.dropdown(
			row,
			"LVRR.PC.OPENVR",
			Translation::from_translation_key(current),
			cells,
			move |position, cells| tasks.push(Task::OpenMenu(position, cells)),
		)?;

		// Steam launch options
		let steam = ui.group(self.id_content, "LVRR.PC.STEAM_COMMAND", "@/dashboard/games.svg")?;
		if state.steam_command.is_empty() {
			ui.muted(steam, Text::Key("LVRR.PC.STEAM_COMMAND_NONE"))?;
		} else {
			let row = ui.row(steam)?;
			ui.bold(row, Text::Raw(&state.steam_command), 14)?;
			let (tasks, command) = (self.tasks.clone(), state.steam_command.clone());
			ui.button(row, Text::Key("LVRR.PC.COPY"), "@/dashboard/check.svg", move || {
				tasks.push(Task::Copy(command.clone()));
			})?;
		}

		let row = ui.row(self.id_content)?;
		ui.muted(row, Text::Key("LVRR.PC.SERVER_APPLIES"))?;
		let tasks = self.tasks.clone();
		ui.button(
			row,
			Text::Key("LVRR.RESTART_SESSION"),
			"@/dashboard/refresh.svg",
			move || {
				tasks.push(Task::Confirm(
					"LVRR.RESTART_SESSION_CONFIRM",
					LvrrRequest::RestartSession,
				));
			},
		)?;
		Ok(())
	}

	fn diagnostics(&self, ui: &mut Ui) -> anyhow::Result<()> {
		let group = ui.group(self.id_content, "LVRR.PC.DIAGNOSTICS", "@/dashboard/diagnostics.svg")?;
		let tasks = self.tasks.clone();
		ui.button(
			group,
			Text::Key("LVRR.PC.RUN_CHECKS"),
			"@/dashboard/refresh.svg",
			move || {
				tasks.push(Task::RunChecks);
			},
		)?;
		match &self.checks {
			None if self.checks_running.is_some() => ui.muted(group, Text::Key("LVRR.LIBRARY.LOADING"))?,
			None => {}
			Some(Err(_)) => ui.muted(group, Text::Key("LVRR.PC.CHECKS_UNAVAILABLE"))?,
			Some(Ok(report)) => {
				for check in &report.checks {
					let row = ui.line(group)?;
					let (badge, color) = match check.status {
						Status::Ok => ("LVRR.PC.CHECK_OK", "success"),
						Status::Warn => ("LVRR.PC.CHECK_WARN", "tertiary"),
						Status::Fail => ("LVRR.PC.CHECK_FAIL", "danger"),
					};
					ui.badge(row, badge, color)?;
					let column = ui.column(row)?;
					ui.bold(column, Text::Raw(&check.title), 14)?;
					ui.muted(column, Text::Raw(&check.message))?;
					if let Some(fix) = &check.fix {
						ui.muted(column, Text::Raw(fix))?;
					}
				}
			}
		}
		Ok(())
	}

	fn about(&mut self, ui: &mut Ui) -> anyhow::Result<()> {
		let version = self
			.version
			.get_or_insert_with(|| {
				// prints "lvrr <version>"
				let version = std::process::Command::new("lvrr")
					.arg("--version")
					.output()
					.ok()
					.filter(|o| o.status.success())
					.and_then(|o| String::from_utf8(o.stdout).ok())
					.and_then(|v| v.trim().strip_prefix("lvrr ").map(str::to_string));
				version.map_or_else(|| "LVRR".into(), |v| format!("LVRR {v}"))
			})
			.clone();
		let group = ui.group(self.id_content, "LVRR.PC.ABOUT", "@/dashboard/info.svg")?;
		ui.bold(group, Text::Raw(&version), 18)?;
		ui.text(group, Text::Key("LVRR.PC.CREDITS"))?;
		let tasks = self.tasks.clone();
		ui.button(
			group,
			Text::Key("LVRR.PC.SUPPORT_UPSTREAM"),
			"@/dashboard/donate.svg",
			move || {
				tasks.push(Task::Frontend(FrontendTask::SetTab(TabType::Donate)));
			},
		)?;
		Ok(())
	}
}
