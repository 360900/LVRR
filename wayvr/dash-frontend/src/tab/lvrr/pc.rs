//! The PC side of LVRR: performance, paired headsets, server settings, system checks.
//! This replaces WiVRn's desktop dashboard.

use std::{
	marker::PhantomData,
	rc::Rc,
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use lvrr_common::{
	doctor::{Check, Report, Status},
	headset::{DecoderTiming, HeadsetStats},
	server_config::{ENCODERS, Encoder, OpenVrCompat, ServerConfig},
	state::{KnownKey, Link, LvrrRequest, LvrrState},
	steam_env,
};
use serde_json::{Value, json};
use wgui::{
	assets::AssetPathRef,
	components::{
		bar_graph::{ComponentBarGraph, ValueCell},
		editbox::ComponentEditBox,
		slider::ComponentSlider,
		tabs::ComponentTabs,
	},
	drawing::Color,
	i18n::Translation,
	layout::{Layout, WidgetID},
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

const ACTION_PREFIX: &str = "lvrr_pc;";

/// Longer than a click, from the press to the release
const CLICK: Duration = Duration::from_secs(1);

thread_local! {
	/// Section to open first, for uidev (`LVRR_PAGE=pc:server`)
	pub(crate) static INITIAL_SECTION: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
	/// Start pairing once the page opens, for the desktop window's tray
	pub(crate) static START_PAIRING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
	/// The server settings as they were before this page first changed one during a session, with
	/// that session's `LvrrState::sessions`. Kept while another page is open or the window is
	/// closed, where the session can restart unseen; in VR, WayVR starts again with the session.
	static SESSION_CONFIG: std::cell::RefCell<Option<(u64, Value)>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn section_name(name: &str) -> Option<&'static str> {
	Section::from_name(name).map(Section::name)
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
	const ALL: [Section; 5] = [
		Section::Performance,
		Section::Headsets,
		Section::Server,
		Section::Diagnostics,
		Section::About,
	];

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
		Self::ALL.into_iter().find(|s| s.name() == name)
	}
}

/// A change to the server configuration, applied to the latest one when sent
#[derive(Clone, Debug)]
enum ServerChange {
	Encoder(Encoder),
	Bool(&'static str, bool),
	/// The value as it goes in the file, None to remove the key
	Number(&'static str, Option<Value>),
	OpenVr(OpenVrCompat),
}

#[derive(Clone)]
enum Task {
	SetSection(Section),
	Confirm(&'static str, LvrrRequest),
	Server(ServerChange),
	OpenMenu(context_menu::Position, Vec<context_menu::Cell>),
	PerfOverlay(bool),
	Copy(String),
	RunChecks,
	/// Fix the check with this id, then run the checks
	Fix(String),
	Frontend(FrontendTask),
	/// Desktop window: pair a new headset, ending the session first if the user agrees
	Pair,
	/// A paired headset's public key and the field with its new name
	Rename(String, Rc<ComponentEditBox>),
}

struct Graphs {
	cpu: Rc<ComponentBarGraph>,
	gpu: Rc<ComponentBarGraph>,
	network: Rc<ComponentBarGraph>,
	latency: Rc<ComponentBarGraph>,
	motion_to_photon: Rc<ComponentBarGraph>,
	/// The latency's stages, in `LATENCY_STAGES` order
	stages: Vec<WidgetID>,
}

type Stage = (&'static str, fn(&DecoderTiming) -> f32);

const LATENCY_STAGES: [Stage; 5] = [
	("LVRR.PC.ENCODE", |d| d.encode_ms),
	("LVRR.PC.SEND", |d| d.send_ms),
	("LVRR.PC.TRANSFER", |d| d.network_ms),
	("LVRR.PC.DECODE", |d| d.decode_ms),
	("LVRR.PC.DISPLAY", |d| d.display_ms),
];

/// How far Monado's steamvr_lh extrapolates poses while lh-max-extrapolation is unset
/// (wivrn/patches/monado/0010-d-steamvr_lh-Add-LH_MAX_EXTRAPOLATION_MS.patch)
const LH_MAX_EXTRAPOLATION_MS: f64 = 50.0;

/// A Lighthouse setting's key, label and slider range, what an unset key means, and what a value
/// is written as
type LighthouseSlider = (
	&'static str,
	&'static str,
	(f32, f32, f32),
	f64,
	fn(f32) -> Option<Value>,
);

const LIGHTHOUSE_SLIDERS: [LighthouseSlider; 2] = [
	// 0 turns extrapolation off, so it is written too, as the integer the server reads
	(
		"lh-max-extrapolation",
		"LVRR.PC.LH_EXTRAPOLATION",
		(0.0, 100.0, 1.0),
		LH_MAX_EXTRAPOLATION_MS,
		|v| Some(json!(v.round() as i64)),
	),
	// 0 is Monado's default and leaves the key out. Rounded, as the f32 0.05 would land in the file
	// as 0.05000000074505806.
	("lh-stick-deadzone", "LVRR.PC.LH_DEADZONE", (0.0, 0.5, 0.01), 0.0, |v| {
		let v = (f64::from(v) * 100.0).round() / 100.0;
		(v > 0.0).then(|| json!(v))
	}),
];

type ChecksResult = Arc<Mutex<Option<Result<Report, String>>>>;

/// What the open section shows of the state. It is built again only when this changes, as a
/// rebuild for a toast or another section's change could land between a control's press and release.
#[derive(PartialEq)]
enum Shown {
	Performance {
		link: Link,
		/// Start LVRR or Pair headset starts LVRR, which changes no state
		starting: bool,
		perf_overlay: Option<bool>,
		/// Graphs rather than a note that there are no statistics
		streaming: bool,
		bitrate_bps: u32,
	},
	Headsets {
		link: Link,
		starting: bool,
		pairing_enabled: bool,
		pin: String,
		known_keys: Vec<KnownKey>,
		/// Pair headset waits for a PIN
		pairing: bool,
	},
	Server {
		link: Link,
		starting: bool,
		server_config: Value,
		/// The server's. What the running Steam lacks is added when the section is built.
		steam_command: String,
		/// A setting changed here waits for a session restart
		restart: bool,
	},
	Diagnostics,
	About,
}

impl Shown {
	fn new(section: Section, state: &LvrrState, perf_overlay: Option<bool>, pairing: bool, starting: bool) -> Self {
		match section {
			Section::Performance => Self::Performance {
				link: state.link,
				starting,
				perf_overlay,
				streaming: streaming(state),
				bitrate_bps: state.bitrate_bps,
			},
			Section::Headsets => Self::Headsets {
				link: state.link,
				starting,
				pairing_enabled: state.pairing_enabled,
				pin: state.pin.clone(),
				known_keys: state.known_keys.clone(),
				pairing,
			},
			Section::Server => Self::Server {
				link: state.link,
				starting,
				server_config: state.server_config.clone(),
				steam_command: state.steam_command.clone(),
				restart: restart_due(state),
			},
			Section::Diagnostics => Self::Diagnostics,
			Section::About => Self::About,
		}
	}
}

pub struct TabLvrrPc<T> {
	state: ParserState,
	widgets: Option<ParserState>,
	tasks: Tasks<Task>,
	id_content: WidgetID,
	section: Section,
	/// LvrrState generation last compared with `shown`, None to compare again
	rendered: Option<u64>,
	/// None to build the section again
	shown: Option<Shown>,
	/// When a headset name field last had the focus
	typed: Option<Instant>,
	/// The `stats_generation` and `stats_live` the graphs and stages show
	stats_rendered: Option<(u64, bool)>,
	graphs: Option<Graphs>,
	pending: Vec<ServerChange>,
	/// The sliders shown, to leave one alone while it is dragged
	sliders: Vec<Rc<ComponentSlider>>,
	context_menu: ContextMenu,
	checks: Option<Result<Report, String>>,
	checks_running: Option<ChecksResult>,
	/// The running checks follow a fix
	fixing: bool,
	/// Pair headset waits for a PIN, from the interface
	pairing: bool,
	/// LVRR starts, from the interface
	starting: bool,
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
					self.invalidate();
					self.typed = None;
				}
				Task::Confirm(message, request) => frontend.lvrr_confirm(message, request),
				Task::Server(change) => {
					// a drag only needs its last value
					if let ServerChange::Number(key, _) = change {
						self
							.pending
							.retain(|c| !matches!(c, ServerChange::Number(k, _) if *k == key));
					}
					self.pending.push(change);
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
				Task::RunChecks => self.run_checks(None),
				Task::Fix(id) => self.run_checks(Some(id)),
				Task::Frontend(task) => frontend.tasks.push(task),
				// the section shows it; with the session still running, a second click would ask again
				Task::Pair if frontend.interface.lvrr_pairing() => {}
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

		// A slider is sent once released, and not rebuilt under the pointer while held
		let dragging = self.sliders.iter().any(|s| s.is_dragging());
		if !dragging && !self.pending.is_empty() {
			self.send_server(frontend, data);
		}

		if let TickResult::Action(name) = self.context_menu.tick(&mut frontend.layout, &mut self.state)?
			&& let Some(change) = parse_action(&name)
		{
			self.tasks.push(Task::Server(change));
		}

		let finished = self.checks_running.as_ref().and_then(|r| r.lock().unwrap().take());
		if let Some(result) = finished {
			// why a fix failed, which the report doesn't show; also on another section
			for error in result.iter().flat_map(|report| &report.errors) {
				let text = frontend
					.layout
					.state
					.globals
					.i18n()
					.translate_and_replace("LVRR.PC.FIX_FAILED", ("{ERROR}", error));
				frontend
					.tasks
					.push(FrontendTask::PushToast(Translation::from_raw_text_string(text)));
			}
			self.checks = Some(result);
			self.checks_running = None;
			self.fixing = false;
			// only Diagnostics shows them
			if self.section == Section::Diagnostics {
				self.invalidate();
			}
		}

		// A rebuild would wipe a headset name being typed. The button that ends it takes the focus
		// on press but clicks on release, where Rename reads the field, so wait for that too.
		if self.section == Section::Headsets && frontend.layout.has_focus() {
			self.typed = Some(Instant::now());
		}
		let typing = self.typed.is_some_and(|t| t.elapsed() < CLICK);

		let (generation, stats_generation, stats_live) = frontend
			.interface
			.lvrr_state(data)
			.map_or((0, 0, false), |s| (s.generation, s.stats_generation, s.stats_live));
		// a pause in the samples changes only stats_live, which keeps the graphs
		if self.stats_rendered != Some((stats_generation, stats_live)) {
			// A step per sample and one per clearing. Samples keep coming while the dashboard is hidden.
			let missed = self
				.stats_rendered
				.map_or(usize::MAX, |(last, _)| stats_generation.saturating_sub(last) as usize);
			self.stats_rendered = Some((stats_generation, stats_live));
			if self.section == Section::Performance
				&& let Some(state) = frontend.interface.lvrr_state(data)
			{
				if self.graphs.is_some() != streaming(state) {
					// the first sample brings the graphs, the end of the stream the note
					self.invalidate();
				} else if let Some(graphs) = &self.graphs {
					let len = state.stats.len();
					if missed > len {
						// cleared since, or more than the history holds: rebuild from it
						self.invalidate();
					} else {
						for stats in state.stats.range(len - missed..) {
							push_graphs(graphs, stats);
						}
						set_stages(&mut frontend.layout, graphs, state.latest_stats());
						frontend.layout.mark_redraw();
					}
				}
			}
		}
		// LVRR starting, also for a pairing, changes no state, nor a start that failed
		let pairing = frontend.interface.lvrr_pairing();
		let starting = frontend.interface.lvrr_starting();
		if self.pairing != pairing || self.starting != starting {
			self.pairing = pairing;
			self.starting = starting;
			self.rendered = None;
		}
		if !dragging && !typing && self.pending.is_empty() && self.rendered != Some(generation) {
			self.rendered = Some(generation);
			// the overlay is drawn in VR, the desktop window can't show it
			let perf_overlay = (!frontend.interface.lvrr_desktop()).then(|| frontend.interface.lvrr_perf_overlay(data));
			let shown = Shown::new(
				self.section,
				frontend.interface.lvrr_state(data).unwrap_or(&LvrrState::default()),
				perf_overlay,
				self.pairing,
				self.starting,
			);
			if self.shown.as_ref() != Some(&shown) {
				self.render(frontend, data, perf_overlay)?;
				self.shown = Some(shown);
				// the graphs were built from every sample so far
				self.stats_rendered = Some((stats_generation, stats_live));
			}
		}
		Ok(())
	}
}

fn parse_action(name: &str) -> Option<ServerChange> {
	let (kind, value) = name.strip_prefix(ACTION_PREFIX)?.split_once(';')?;
	Some(match kind {
		"encoder" if value == "auto" => ServerChange::Encoder(Encoder::Automatic),
		"encoder" => ServerChange::Encoder(Encoder::Named(value.to_string())),
		"openvr" => ServerChange::OpenVr(match value {
			"unmanaged" => OpenVrCompat::Unmanaged,
			_ => OpenVrCompat::Automatic,
		}),
		_ => return None,
	})
}

/// An encoder with the GPUs it runs on: the session fails with one the PC can't run
fn encoder_label(name: &str) -> Translation {
	let key = match name {
		"nvenc" => "LVRR.PC.ENCODER_NVENC",
		"vaapi" => "LVRR.PC.ENCODER_VAAPI",
		"vulkan" => "LVRR.PC.ENCODER_VULKAN",
		"x264" => "LVRR.PC.ENCODER_X264",
		// written in the config file by hand
		_ => return Translation::from_raw_text(name),
	};
	Translation::from_translation_key(key)
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

/// Graphs rather than a note that there are no statistics. Like Home, only with a headset
/// connected: a sample may come after it left.
fn streaming(state: &LvrrState) -> bool {
	state.headset_connected && !state.stats.is_empty()
}

/// What a session reads of the server settings when it starts: all but USB connection, which the
/// server applies at once
fn session_config(config: &Value) -> Value {
	let mut config = ServerConfig(config.clone());
	config.set("usb-auto-connect", None);
	config.0
}

/// A server setting changed here waits for the running session to restart. A copy saved during an
/// earlier session doesn't count, the running session read the change when it started.
fn restart_due(state: &LvrrState) -> bool {
	let config = session_config(&state.server_config);
	state.session_running
		&& SESSION_CONFIG.with_borrow(|saved| {
			saved
				.as_ref()
				.is_some_and(|(sessions, s)| *sessions == state.sessions && *s != config)
		})
}

/// The latency's stages, of the latest sample only: "-" while the headset sends none, as on Home
fn set_stages(layout: &mut Layout, graphs: &Graphs, stats: Option<&HeadsetStats>) {
	let decoder = stats.and_then(HeadsetStats::slowest_decoder);
	for (&id, (_, get)) in graphs.stages.iter().zip(LATENCY_STAGES) {
		let text = decoder.map_or_else(|| "-".to_string(), |d| format!("{:.1} ms", get(d)));
		ui::set_label(layout, id, Translation::from_raw_text_string(text));
	}
}

fn push_graphs(graphs: &Graphs, stats: &HeadsetStats) {
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
			for s in Section::ALL {
				if let Some(button) = tabs.get_tab_button(s.name()) {
					button.set_sticky_state(&mut common, s == section);
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
			shown: None,
			typed: None,
			stats_rendered: None,
			graphs: None,
			pending: Vec::new(),
			sliders: Vec::new(),
			context_menu: ContextMenu::default(),
			checks: None,
			checks_running: None,
			fixing: false,
			pairing: false,
			starting: false,
			version: None,
			marker: PhantomData,
		})
	}

	/// Builds the section again, also if nothing `Shown` holds changed
	fn invalidate(&mut self) {
		self.rendered = None;
		self.shown = None;
	}

	fn send_server(&mut self, frontend: &mut Frontend<T>, data: &mut T) {
		// the controls show the change already; the next state rebuilds them even if it matches what
		// was shown, so a change the server didn't take doesn't stay on screen
		self.shown = None;
		let Some(state) = frontend.interface.lvrr_state(data) else {
			self.pending.clear();
			return;
		};
		// the server couldn't read its file, which these changes alone would replace
		if !state.server_config.is_object() {
			self.pending.clear();
			return;
		}
		// the running session keeps what it read, the restart banner compares with it; a copy from
		// an earlier session goes
		if state.session_running {
			SESSION_CONFIG.with_borrow_mut(|saved| {
				if saved.as_ref().is_none_or(|(sessions, _)| *sessions != state.sessions) {
					*saved = Some((state.sessions, session_config(&state.server_config)));
				}
			});
		}
		let mut config = ServerConfig(state.server_config.clone());
		for change in self.pending.drain(..) {
			match change {
				ServerChange::Encoder(encoder) => config.set_encoder(&encoder),
				ServerChange::Bool(key, value) => config.set(key, value.then_some(Value::Bool(true))),
				ServerChange::Number(key, value) => config.set(key, value),
				ServerChange::OpenVr(compat) => config.set_openvr_compat(&compat),
			}
		}
		frontend
			.tasks
			.push(FrontendTask::Lvrr(LvrrRequest::SetServerConfig(config.0)));
	}

	/// Runs the checks, after the fix of the check `fix` names
	fn run_checks(&mut self, fix: Option<String>) {
		if self.checks_running.is_some() {
			return;
		}
		let result: ChecksResult = Arc::default();
		let out = result.clone();
		self.fixing = fix.is_some();
		std::thread::spawn(move || {
			let report = match fix {
				Some(id) => Report::fix(&id),
				None => Report::run(),
			};
			let report = report.inspect_err(|e| log::warn!("Could not run the checks: {e:?}"));
			*out.lock().unwrap() = Some(report.map_err(|e| format!("{e:#}")));
		});
		self.checks_running = Some(result);
		// lvrr doctor can take seconds: show that it runs, not an earlier report it may repeat
		self.checks = None;
		self.invalidate();
	}

	fn render(&mut self, frontend: &mut Frontend<T>, data: &mut T, perf_overlay: Option<bool>) -> anyhow::Result<()> {
		self.graphs = None;
		self.sliders.clear();
		let state = frontend.interface.lvrr_state(data).cloned().unwrap_or_default();
		let desktop = frontend.interface.lvrr_desktop();

		frontend.layout.remove_children(self.id_content);
		let globals = frontend.layout.state.globals.clone();
		let mut widgets =
			wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, self.id_content)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut widgets, &globals);

		// Without the server, Headsets and Server would show stale values and controls that only
		// fail; Diagnostics can tell why it isn't running. Pair headset starts it.
		let online = state.link == Link::Online;
		if !online && self.pairing && self.section == Section::Headsets {
			ui.banner(self.id_content, "LVRR.PC.PAIR_WAIT", "primary")?;
		} else if !online && matches!(self.section, Section::Performance | Section::Headsets | Section::Server) {
			let tasks = self.tasks.clone();
			ui.not_running(self.id_content, state.link, desktop, self.starting, move || {
				tasks.push(Task::Frontend(FrontendTask::LvrrStartServer));
			})?;
		}
		match self.section {
			Section::Performance => self.graphs = self.performance(&mut ui, &state, perf_overlay)?,
			Section::Headsets | Section::Server if !online => {}
			Section::Headsets => self.headsets(&mut ui, &state, desktop)?,
			Section::Server => self.sliders = self.server(&mut ui, &state)?,
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
		if !streaming(state) {
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
			stages: Vec::new(),
		};
		ui.muted(group, Text::Key("LVRR.PC.LATENCY_STAGES"))?;
		let row = ui.row(group)?;
		let stages = LATENCY_STAGES
			.iter()
			.map(|(key, _)| ui.stat(row, key))
			.collect::<anyhow::Result<_>>()?;
		let graphs = Graphs { stages, ..graphs };
		for stats in &state.stats {
			push_graphs(&graphs, stats);
		}
		set_stages(ui.layout, &graphs, state.latest_stats());
		Ok(Some(graphs))
	}

	/// The PIN while pairing, and the paired headsets with a field for each name. The desktop
	/// window can also start pairing.
	fn headsets(&self, ui: &mut Ui, state: &LvrrState, desktop: bool) -> anyhow::Result<()> {
		let pin = state.pairing_enabled && !state.pin.is_empty();
		if desktop {
			if pin {
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
			} else if self.pairing {
				// where the PIN comes next
				ui.banner(self.id_content, "LVRR.PC.PAIR_WAIT", "primary")?;
			} else {
				let row = ui.row(self.id_content)?;
				let tasks = self.tasks.clone();
				ui.button(row, Text::Key("LVRR.PC.PAIR"), "@/dashboard/add.svg", move || {
					tasks.push(Task::Pair);
				})?;
				ui.muted(row, Text::Key("LVRR.PC.PAIR_HOWTO_DESKTOP"))?;
			}
		} else if pin {
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
			let field = ui.editbox(column, &key.name)?;
			match key.last_connection {
				Some(t) => {
					// its own statement, as building the label needs the globals this borrows
					let text = ui
						.layout
						.state
						.globals
						.i18n()
						.translate_and_replace("LVRR.PC.LAST_CONNECTED", ("{TIME}", &format_time(t)));
					ui.muted(column, Text::Raw(&text))?;
				}
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
		if !desktop {
			ui.muted(group, Text::Key("LVRR.PC.PAIR_HOWTO"))?;
		}
		Ok(())
	}

	/// Returns the sliders it built
	fn server(&self, ui: &mut Ui, state: &LvrrState) -> anyhow::Result<Vec<Rc<ComponentSlider>>> {
		// The server publishes null for a file it couldn't parse and runs with its defaults. The
		// controls would show those as the file's settings, and a change would replace the file.
		if !state.server_config.is_object() {
			let banner = ui.banner(self.id_content, "LVRR.PC.SERVER_CONFIG_UNREADABLE", "danger")?;
			if let Some(path) = lvrr_common::server_config::path() {
				ui.bold(banner, Text::Raw(&path.display().to_string()), 14)?;
			}
			self.steam_command(ui, state)?;
			return Ok(Vec::new());
		}
		let config = ServerConfig(state.server_config.clone());
		if restart_due(state) {
			let tasks = self.tasks.clone();
			ui.restart_banner(self.id_content, move || {
				tasks.push(Task::Confirm(
					"LVRR.RESTART_SESSION_CONFIRM",
					LvrrRequest::RestartSession,
				));
			})?;
		}
		let group = ui.group(self.id_content, "LVRR.PC.SERVER", "@/dashboard/cpu.svg")?;

		// encoder
		let row = ui.row(group)?;
		let current = match config.encoder() {
			Encoder::Automatic => Translation::from_translation_key("LVRR.HEADSET.AUTOMATIC"),
			Encoder::Named(name) => encoder_label(&name),
			Encoder::Custom => Translation::from_translation_key("LVRR.PC.ENCODER_CUSTOM"),
		};
		let mut cells = vec![ui::menu_cell(
			format!("{ACTION_PREFIX}encoder;auto"),
			Translation::from_translation_key("LVRR.HEADSET.AUTOMATIC"),
		)];
		cells.extend(
			ENCODERS
				.iter()
				.map(|e| ui::menu_cell(format!("{ACTION_PREFIX}encoder;{e}"), encoder_label(e))),
		);
		let tasks = self.tasks.clone();
		// fits the longest name, VA-API's, so the box keeps its width when another encoder is picked
		ui.dropdown_sized(row, "LVRR.PC.ENCODER", current, cells, 210, move |position, cells| {
			tasks.push(Task::OpenMenu(position, cells));
		})?;

		// toggles
		for (key, label) in [
			("hid-forwarding", "LVRR.PC.HID_FORWARDING"),
			("usb-auto-connect", "LVRR.PC.USB_AUTO_CONNECT"),
			("use-steamvr-lh", "LVRR.PC.STEAMVR_LH"),
		] {
			let tasks = self.tasks.clone();
			ui.toggle(group, Text::Key(label), config.bool(key), move |v| {
				tasks.push(Task::Server(ServerChange::Bool(key, v)));
			})?;
		}

		// Lighthouse tuning
		let mut sliders = Vec::new();
		if config.bool("use-steamvr-lh") {
			for (key, label, range, unset, to_json) in LIGHTHOUSE_SLIDERS {
				let tasks = self.tasks.clone();
				let value = config.number(key).unwrap_or(unset) as f32;
				let slider = ui.slider(group, label, None, range, value, move |v| {
					tasks.push(Task::Server(ServerChange::Number(key, to_json(v))));
				})?;
				sliders.push(slider);
			}
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
		ui.muted(group, Text::Key("LVRR.PC.SERVER_APPLIES"))?;
		self.steam_command(ui, state)?;
		Ok(sliders)
	}

	fn steam_command(&self, ui: &mut Ui, state: &LvrrState) -> anyhow::Result<()> {
		let steam = ui.group(self.id_content, "LVRR.PC.STEAM_COMMAND", "@/dashboard/games.svg")?;
		// The server judges by its own environment, but games get the running Steam's: add the import it lacks, as
		// `lvrr steam` does. Read when the section is built.
		let command = steam_env::with_openxr_import(&state.steam_command, steam_env::client_import());
		if command.is_empty() {
			ui.muted(steam, Text::Key("LVRR.PC.STEAM_COMMAND_NONE"))?;
		} else {
			let row = ui.row(steam)?;
			ui.bold(row, Text::Raw(&command), 14)?;
			let tasks = self.tasks.clone();
			ui.button(row, Text::Key("LVRR.PC.COPY"), "@/dashboard/check.svg", move || {
				tasks.push(Task::Copy(command.clone()));
			})?;
		}
		Ok(())
	}

	fn diagnostics(&self, ui: &mut Ui) -> anyhow::Result<()> {
		let group = ui.group(self.id_content, "LVRR.PC.DIAGNOSTICS", "@/dashboard/diagnostics.svg")?;
		let row = ui.row(group)?;
		let tasks = self.tasks.clone();
		ui.button(
			row,
			Text::Key("LVRR.PC.RUN_CHECKS"),
			"@/dashboard/refresh.svg",
			move || {
				tasks.push(Task::RunChecks);
			},
		)?;
		let fixable = |check: &Check| check.status != Status::Ok && check.fix.is_some();
		// next to the button: the rows that need it come first, and Fixing... replaces the list while
		// the password prompt waits
		if self.fixing || matches!(&self.checks, Some(Ok(report)) if report.checks.iter().any(fixable)) {
			ui.muted(row, Text::Key("LVRR.PC.FIX_ADMIN"))?;
		}
		match &self.checks {
			None if self.fixing => ui.muted(group, Text::Key("LVRR.PC.FIXING"))?,
			None if self.checks_running.is_some() => ui.muted(group, Text::Key("LVRR.PC.CHECKS_RUNNING"))?,
			None => {}
			Some(Err(e)) => {
				ui.muted(group, Text::Key("LVRR.PC.CHECKS_UNAVAILABLE"))?;
				ui.muted(group, Text::Raw(e))?;
			}
			Some(Ok(report)) => {
				for check in &report.checks {
					let row = ui.line(group)?;
					let (badge, color) = match check.status {
						Status::Ok => ("LVRR.PC.CHECK_OK", "success"),
						// amber in the default palette, where tertiary is a teal that reads as OK's green
						Status::Warn => ("LVRR.PC.CHECK_WARN", "secondary"),
						Status::Fail => ("LVRR.PC.CHECK_FAIL", "danger"),
					};
					ui.badge(row, badge, color)?;
					let column = ui.column(row)?;
					ui.bold(column, Text::Raw(&check.title), 14)?;
					ui.muted(column, Text::Raw(&check.message))?;
					// a button rather than the fix's command, as VR has no terminal
					if fixable(check) {
						let (tasks, id) = (self.tasks.clone(), check.id.clone());
						ui.button(row, Text::Key("LVRR.PC.FIX"), "@/dashboard/magic_wand.svg", move || {
							tasks.push(Task::Fix(id.clone()));
						})?;
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
				let version = std::process::Command::new(lvrr_common::lvrr_command())
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
