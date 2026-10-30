//! Headset settings, which the settings window on the headset used to show.

use std::{
	cell::RefCell,
	collections::BTreeMap,
	marker::PhantomData,
	rc::Rc,
	time::{Duration, Instant},
};

use lvrr_common::{
	headset::{BODY_PARTS, Codec, Config, HeadsetSettings, PostProcessing, Posture},
	state::LvrrRequest,
};
use serde_json::{Value, json};
use wgui::{
	assets::AssetPathRef,
	components::{slider::ComponentSlider, tabs::ComponentTabs},
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

/// Sliders send their value once they stop moving
const DEBOUNCE: Duration = Duration::from_millis(400);
const ACTION_PREFIX: &str = "lvrr;";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Section {
	Video,
	Streaming,
	PostProcessing,
	Audio,
	Devices,
	Tracking,
	System,
}

impl Section {
	const fn name(self) -> &'static str {
		match self {
			Section::Video => "video",
			Section::Streaming => "streaming",
			Section::PostProcessing => "post_processing",
			Section::Audio => "audio",
			Section::Devices => "devices",
			Section::Tracking => "tracking",
			Section::System => "system",
		}
	}

	fn from_name(name: &str) -> Option<Self> {
		Some(match name {
			"video" => Section::Video,
			"streaming" => Section::Streaming,
			"post_processing" => Section::PostProcessing,
			"audio" => Section::Audio,
			"devices" => Section::Devices,
			"tracking" => Section::Tracking,
			"system" => Section::System,
			_ => return None,
		})
	}

	const fn title_icon(self) -> (&'static str, &'static str) {
		match self {
			Section::Video => ("LVRR.HEADSET.VIDEO", "@/dashboard/display.svg"),
			Section::Streaming => ("LVRR.HEADSET.STREAMING", "@/dashboard/usage.svg"),
			Section::PostProcessing => ("LVRR.HEADSET.POST_PROCESSING", "@/dashboard/magic_wand.svg"),
			Section::Audio => ("LVRR.HEADSET.AUDIO", "@/dashboard/microphone.svg"),
			Section::Devices => ("LVRR.HEADSET.DEVICES", "@/dashboard/controller.svg"),
			Section::Tracking => ("LVRR.HEADSET.TRACKING", "@/dashboard/person.svg"),
			Section::System => ("LVRR.HEADSET.SYSTEM", "@/dashboard/options.svg"),
		}
	}
}

type Options = Vec<(Value, Translation)>;

enum Control {
	Toggle {
		get: fn(&Config) -> bool,
		set: fn(bool) -> Value,
	},
	/// Shown in display units, sent in headset units
	Slider {
		range: fn(&HeadsetSettings) -> (f32, f32, f32),
		get: fn(&Config) -> f32,
		set: fn(f32) -> Value,
	},
	Choice {
		options: fn(&HeadsetSettings) -> Options,
		get: fn(&Config) -> Value,
	},
	BodyParts,
}

struct Setting {
	/// The key in `HeadsetSettings.config`
	key: &'static str,
	label: &'static str,
	section: Section,
	/// Only applies to a new session
	restart: bool,
	show: fn(&HeadsetSettings) -> bool,
	control: Control,
}

const fn always(_: &HeadsetSettings) -> bool {
	true
}

fn mbit(bps: u32) -> f32 {
	bps as f32 / 1e6
}

fn key(text: &str) -> Translation {
	Translation::from_translation_key(text)
}

fn post_processing_options(_: &HeadsetSettings) -> Options {
	PostProcessing::ALL
		.iter()
		.map(|level| {
			let label = match level {
				PostProcessing::Off => "LVRR.HEADSET.PP_OFF",
				PostProcessing::Normal => "LVRR.HEADSET.PP_NORMAL",
				PostProcessing::Quality => "LVRR.HEADSET.PP_QUALITY",
			};
			(json!(level.key()), key(label))
		})
		.collect()
}

fn toggle(get: fn(&Config) -> bool) -> Control {
	Control::Toggle { get, set: |v| json!(v) }
}

const PRESET_ACTION_PREFIX: &str = "lvrr-preset;";

/// Sets resolution, foveation, bitrate and codec at once
struct Preset {
	label: &'static str,
	resolution_scale: f32,
	foveation: f32,
	bitrate_mbit: u32,
}

const PRESETS: [Preset; 5] = [
	Preset {
		label: "LVRR.HEADSET.PRESET_POTATO",
		resolution_scale: 0.8,
		foveation: 0.7,
		bitrate_mbit: 30,
	},
	Preset {
		label: "LVRR.HEADSET.PRESET_LOW",
		resolution_scale: 1.0,
		foveation: 0.6,
		bitrate_mbit: 40,
	},
	Preset {
		label: "LVRR.HEADSET.PRESET_MEDIUM",
		resolution_scale: 1.2,
		foveation: 0.5,
		bitrate_mbit: 50,
	},
	Preset {
		label: "LVRR.HEADSET.PRESET_HIGH",
		resolution_scale: 1.4,
		foveation: 0.4,
		bitrate_mbit: 80,
	},
	Preset {
		label: "LVRR.HEADSET.PRESET_ULTRA",
		resolution_scale: 1.5,
		foveation: 0.3,
		bitrate_mbit: 120,
	},
];

impl Preset {
	/// As the headset stores it
	fn values(&self, h: &HeadsetSettings) -> (f32, f32, u32) {
		// eye tracking foveates more for the same look, as the headset's default does
		let eye_gaze = h.capabilities.eye_gaze && h.config.eye_gaze;
		let foveation = if eye_gaze {
			(self.foveation + 0.2).min(0.8)
		} else {
			self.foveation
		};
		let bitrate = (self.bitrate_mbit * 1_000_000).min(h.config.max_bitrate_bps(&h.capabilities));
		(self.resolution_scale, foveation, bitrate)
	}

	fn patch(&self, h: &HeadsetSettings) -> [(&'static str, Value); 4] {
		let (resolution_scale, foveation, bitrate) = self.values(h);
		[
			("resolution_scale", json!(resolution_scale)),
			("foveation", json!(foveation)),
			("bitrate_bps", json!(bitrate)),
			("codec", json!(Codec::Automatic.key())),
		]
	}

	fn matches(&self, h: &HeadsetSettings) -> bool {
		let (resolution_scale, foveation, bitrate) = self.values(h);
		let close = |a: f32, b: f32| (a - b).abs() < 0.005;
		close(h.config.resolution_scale, resolution_scale)
			&& close(h.config.foveation, foveation)
			&& h.config.bitrate_bps == bitrate
			&& h.config.codec == Codec::Automatic
	}
}

fn settings() -> Vec<Setting> {
	use Section::*;
	vec![
		Setting {
			key: "preferred_refresh_rate",
			label: "LVRR.HEADSET.REFRESH_RATE",
			section: Video,
			restart: false,
			show: |h| h.capabilities.refresh_rate_control,
			control: Control::Choice {
				options: |h| {
					let mut options = vec![(json!(0.0), key("LVRR.HEADSET.AUTOMATIC"))];
					for rate in &h.capabilities.refresh_rates {
						options.push((
							json!(rate),
							Translation::from_raw_text_string(format!("{} Hz", rate.round())),
						));
					}
					options
				},
				get: |c| json!(c.preferred_refresh_rate),
			},
		},
		Setting {
			key: "fps_divider",
			label: "LVRR.HEADSET.SPACEWARP",
			section: Video,
			restart: false,
			show: |h| h.capabilities.spacewarp && h.config.preferred_refresh_rate != 0.0,
			control: Control::Toggle {
				get: |c| c.fps_divider == 2,
				set: |v| json!(if v { 2 } else { 1 }),
			},
		},
		Setting {
			key: "resolution_scale",
			label: "LVRR.HEADSET.RESOLUTION",
			section: Video,
			restart: true,
			show: always,
			control: Control::Slider {
				range: |h| (50.0, if h.config.extended_config { 350.0 } else { 150.0 }, 5.0),
				get: |c| c.resolution_scale * 100.0,
				set: |v| json!(v / 100.0),
			},
		},
		Setting {
			key: "fov_horizontal",
			label: "LVRR.HEADSET.FOV_HORIZONTAL",
			section: Video,
			restart: false,
			show: always,
			control: Control::Slider {
				range: |_| (70.0, 100.0, 1.0),
				get: |c| c.fov_horizontal * 100.0,
				set: |v| json!(v / 100.0),
			},
		},
		Setting {
			key: "fov_vertical",
			label: "LVRR.HEADSET.FOV_VERTICAL",
			section: Video,
			restart: false,
			show: always,
			control: Control::Slider {
				range: |_| (70.0, 100.0, 1.0),
				get: |c| c.fov_vertical * 100.0,
				set: |v| json!(v / 100.0),
			},
		},
		Setting {
			key: "codec",
			label: "LVRR.HEADSET.CODEC",
			section: Streaming,
			restart: true,
			show: always,
			control: Control::Choice {
				options: |h| {
					let mut options = vec![(json!(Codec::Automatic.key()), key("LVRR.HEADSET.AUTOMATIC"))];
					for codec in &h.capabilities.codecs {
						options.push((json!(codec.key()), Translation::from_raw_text(codec.display_name())));
					}
					options
				},
				get: |c| json!(c.codec.key()),
			},
		},
		Setting {
			key: "foveation",
			label: "LVRR.HEADSET.FOVEATION",
			section: Streaming,
			restart: true,
			show: always,
			control: Control::Slider {
				range: |h| (if h.config.extended_config { 0.0 } else { 30.0 }, 80.0, 5.0),
				get: |c| c.foveation * 100.0,
				set: |v| json!(v / 100.0),
			},
		},
		Setting {
			key: "bit_depth",
			label: "LVRR.HEADSET.TEN_BIT",
			section: Streaming,
			restart: true,
			show: |h| h.config.codec != Codec::Automatic && h.capabilities.codecs_10bit.contains(&h.config.codec),
			control: Control::Toggle {
				get: |c| c.bit_depth == 10,
				set: |v| json!(if v { 10 } else { 8 }),
			},
		},
		Setting {
			key: "bitrate_bps",
			label: "LVRR.HEADSET.BITRATE",
			section: Streaming,
			restart: false,
			show: always,
			control: Control::Slider {
				range: |h| (5.0, mbit(h.config.max_bitrate_bps(&h.capabilities)), 1.0),
				get: |c| mbit(c.bitrate_bps),
				set: |v| json!(v.round() as u64 * 1_000_000),
			},
		},
		Setting {
			key: "foveation_override",
			label: "LVRR.HEADSET.FOVEATION_OVERRIDE",
			section: Streaming,
			restart: false,
			show: always,
			control: toggle(|c| c.foveation_override),
		},
		Setting {
			key: "foveation_pitch",
			label: "LVRR.HEADSET.FOVEATION_HEIGHT",
			section: Streaming,
			restart: false,
			show: |h| h.config.foveation_override,
			control: Control::Slider {
				range: |_| (-45.0, 45.0, 1.0),
				get: |c| -c.foveation_pitch.to_degrees(),
				set: |v| json!((-v).to_radians()),
			},
		},
		Setting {
			key: "foveation_distance",
			label: "LVRR.HEADSET.FOVEATION_DISTANCE",
			section: Streaming,
			restart: false,
			show: |h| h.config.foveation_override,
			control: Control::Slider {
				range: |_| (0.5, 10.0, 0.1),
				get: |c| c.foveation_distance,
				set: |v| json!(v),
			},
		},
		Setting {
			key: "super_sampling",
			label: "LVRR.HEADSET.SUPERSAMPLING",
			section: PostProcessing,
			restart: false,
			show: |h| h.capabilities.post_processing,
			control: Control::Choice {
				options: post_processing_options,
				get: |c| json!(c.super_sampling.key()),
			},
		},
		Setting {
			key: "sharpening",
			label: "LVRR.HEADSET.SHARPENING",
			section: PostProcessing,
			restart: false,
			show: always,
			control: Control::Slider {
				range: |_| (0.0, 100.0, 5.0),
				get: |c| c.sharpening * 100.0,
				set: |v| json!(v / 100.0),
			},
		},
		Setting {
			key: "microphone",
			label: "LVRR.HEADSET.MICROPHONE",
			section: Audio,
			restart: true,
			show: always,
			control: toggle(|c| c.microphone),
		},
		Setting {
			key: "mic_unprocessed",
			label: "LVRR.HEADSET.MIC_UNPROCESSED",
			section: Audio,
			restart: true,
			show: |h| h.capabilities.android && h.config.microphone,
			control: toggle(|c| c.mic_unprocessed),
		},
		Setting {
			key: "forward_keyboard",
			label: "LVRR.HEADSET.FORWARD_KEYBOARD",
			section: Devices,
			restart: false,
			show: |h| h.capabilities.keyboard_mouse,
			control: toggle(|c| c.forward_keyboard),
		},
		Setting {
			key: "forward_mouse",
			label: "LVRR.HEADSET.FORWARD_MOUSE",
			section: Devices,
			restart: false,
			show: |h| h.capabilities.keyboard_mouse,
			control: toggle(|c| c.forward_mouse),
		},
		Setting {
			key: "forward_gamepad",
			label: "LVRR.HEADSET.FORWARD_GAMEPAD",
			section: Devices,
			restart: false,
			show: always,
			control: toggle(|c| c.forward_gamepad),
		},
		Setting {
			key: "hand_tracking",
			label: "LVRR.HEADSET.HAND_TRACKING",
			section: Tracking,
			restart: true,
			show: |h| h.capabilities.hand_tracking,
			control: toggle(|c| c.hand_tracking),
		},
		Setting {
			key: "simultaneous_hands",
			label: "LVRR.HEADSET.SIMULTANEOUS_HANDS",
			section: Tracking,
			restart: false,
			show: |h| h.capabilities.simultaneous_hands,
			control: toggle(|c| c.simultaneous_hands),
		},
		Setting {
			key: "eye_gaze",
			label: "LVRR.HEADSET.EYE_TRACKING",
			section: Tracking,
			restart: true,
			show: |h| h.capabilities.eye_gaze,
			control: toggle(|c| c.eye_gaze),
		},
		Setting {
			key: "face_tracking",
			label: "LVRR.HEADSET.FACE_TRACKING",
			section: Tracking,
			restart: true,
			show: |h| h.capabilities.face_tracking,
			control: toggle(|c| c.face_tracking),
		},
		Setting {
			key: "body_tracking",
			label: "LVRR.HEADSET.BODY_TRACKING",
			section: Tracking,
			restart: true,
			show: |h| h.capabilities.body_tracking,
			control: toggle(|c| c.body_tracking),
		},
		Setting {
			key: "body_parts",
			label: "LVRR.HEADSET.BODY_PARTS",
			section: Tracking,
			restart: false,
			show: |h| h.capabilities.body_tracking && !h.capabilities.body_parts.is_empty(),
			control: Control::BodyParts,
		},
		Setting {
			key: "posture",
			label: "LVRR.HEADSET.POSTURE",
			section: Tracking,
			restart: false,
			show: always,
			control: Control::Choice {
				options: |_| {
					vec![
						(json!(Posture::Standing.key()), key("LVRR.HEADSET.STANDING")),
						(json!(Posture::Seated.key()), key("LVRR.HEADSET.SEATED")),
					]
				},
				get: |c| json!(c.posture.key()),
			},
		},
		Setting {
			key: "height_offset",
			label: "LVRR.HEADSET.HEIGHT",
			section: Tracking,
			restart: false,
			show: always,
			control: Control::Slider {
				range: |_| (-30.0, 70.0, 1.0),
				get: |c| c.height_offset * 100.0,
				set: |v| json!(v / 100.0),
			},
		},
		Setting {
			key: "extended_config",
			label: "LVRR.HEADSET.EXTENDED_CONFIG",
			section: System,
			restart: false,
			show: always,
			control: toggle(|c| c.extended_config),
		},
		Setting {
			key: "high_power_mode",
			label: "LVRR.HEADSET.HIGH_POWER",
			section: System,
			restart: false,
			show: |h| h.capabilities.high_power_mode,
			control: toggle(|c| c.high_power_mode),
		},
		Setting {
			key: "passthrough_boundary",
			label: "LVRR.HEADSET.BOUNDARY",
			section: System,
			restart: false,
			show: |h| h.capabilities.passthrough_boundary,
			control: toggle(|c| c.passthrough_boundary),
		},
		Setting {
			key: "see_through",
			label: "LVRR.HEADSET.SEE_THROUGH",
			section: System,
			restart: false,
			show: |h| h.capabilities.see_through,
			control: Control::Slider {
				range: |_| (0.0, 80.0, 5.0),
				get: |c| c.see_through * 100.0,
				set: |v| json!(v / 100.0),
			},
		},
		Setting {
			key: "usb_network",
			label: "LVRR.HEADSET.USB_NETWORK",
			section: System,
			restart: false,
			show: |h| h.capabilities.usb_network,
			control: toggle(|c| c.usb_network),
		},
	]
}

#[derive(Clone)]
enum Task {
	SetSection(Section),
	Edit {
		key: &'static str,
		value: Value,
		/// false for sliders, which are sent once they settle
		now: bool,
	},
	OpenMenu(context_menu::Position, Vec<context_menu::Cell>),
	/// Index in PRESETS
	Preset(usize),
	RestartSession,
}

thread_local! {
	/// Section to open first, for uidev (`LVRR_PAGE=headset:streaming`)
	pub(crate) static INITIAL_SECTION: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

pub(crate) fn section_name(name: &str) -> Option<&'static str> {
	[
		"video",
		"streaming",
		"post_processing",
		"audio",
		"devices",
		"tracking",
		"system",
	]
	.into_iter()
	.find(|n| *n == name)
}

pub struct TabLvrrHeadset<T> {
	state: ParserState,
	widgets: Option<ParserState>,
	tasks: Tasks<Task>,
	id_content: WidgetID,
	section: Section,
	/// LvrrState generation shown, None to render again
	rendered: Option<u64>,
	pending: BTreeMap<&'static str, Value>,
	last_edit: Instant,
	/// The sliders shown, to leave one alone while it is dragged
	sliders: Vec<Rc<ComponentSlider>>,
	context_menu: ContextMenu,
	marker: PhantomData<T>,
}

impl<T: 'static> Tab<T> for TabLvrrHeadset<T> {
	fn get_type(&self) -> TabType {
		TabType::Lvrr(Page::Headset)
	}

	fn update(&mut self, frontend: &mut Frontend<T>, _time_ms: u32, data: &mut T) -> anyhow::Result<()> {
		for task in self.tasks.drain() {
			match task {
				Task::SetSection(section) => {
					self.section = section;
					super::set_section(section.name());
					self.rendered = None;
				}
				Task::Edit { key, value, now } => {
					self.pending.insert(key, value);
					self.last_edit = Instant::now();
					if now {
						self.send(frontend);
					}
				}
				Task::OpenMenu(position, cells) => self.context_menu.open(context_menu::OpenParams {
					on_custom_attribs: None,
					position,
					blueprint: Blueprint::Cells(cells),
				}),
				Task::Preset(index) => {
					if let Some(h) = frontend.interface.lvrr_state(data).and_then(|s| s.headset.clone()) {
						self.pending.extend(PRESETS[index].patch(&h));
						self.send(frontend);
					}
				}
				Task::RestartSession => frontend.lvrr_confirm("LVRR.RESTART_SESSION_CONFIRM", LvrrRequest::RestartSession),
			}
		}

		// A dragged slider is neither sent nor rebuilt under the pointer
		let dragging = self.sliders.iter().any(|s| s.is_dragging());
		if !dragging && !self.pending.is_empty() && self.last_edit.elapsed() >= DEBOUNCE {
			self.send(frontend);
		}

		if let TickResult::Action(name) = self.context_menu.tick(&mut frontend.layout, &mut self.state)? {
			if let Some(index) = name
				.strip_prefix(PRESET_ACTION_PREFIX)
				.and_then(|i| i.parse::<usize>().ok())
				.filter(|i| *i < PRESETS.len())
			{
				self.tasks.push(Task::Preset(index));
			} else if let Some((key, value)) = parse_action(&name) {
				self.tasks.push(Task::Edit { key, value, now: true });
			}
		}

		// Show the headset's answer, unless the user is still moving a slider
		let generation = frontend.interface.lvrr_state(data).map_or(0, |s| s.generation);
		if !dragging && self.pending.is_empty() && self.rendered != Some(generation) {
			self.render(frontend, data, generation)?;
		}
		Ok(())
	}
}

fn parse_action(name: &str) -> Option<(&'static str, Value)> {
	let (key, value) = name.strip_prefix(ACTION_PREFIX)?.split_once(';')?;
	let key = settings().iter().find(|s| s.key == key)?.key;
	Some((key, serde_json::from_str(value).ok()?))
}

impl<T: 'static> TabLvrrHeadset<T> {
	pub fn new(frontend: &mut Frontend<T>, parent: WidgetID, _data: &mut T) -> anyhow::Result<Self> {
		let globals = frontend.layout.state.globals.clone();
		let state = wgui::parser::parse_from_assets(
			&ParseDocumentParams {
				globals,
				path: AssetPathRef::BuiltIn("gui/lvrr/headset.xml"),
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
			.unwrap_or(Section::Video);
		super::set_section(section.name());

		let tabs = state.fetch_component_as::<ComponentTabs>("tabs")?;
		if section != Section::Video {
			let mut common = frontend.layout.common();
			for name in [
				"video",
				"streaming",
				"post_processing",
				"audio",
				"devices",
				"tracking",
				"system",
			] {
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
			pending: BTreeMap::new(),
			last_edit: Instant::now(),
			sliders: Vec::new(),
			context_menu: ContextMenu::default(),
			marker: PhantomData,
		})
	}

	fn send(&mut self, frontend: &mut Frontend<T>) {
		if self.pending.is_empty() {
			return;
		}
		let patch = std::mem::take(&mut self.pending)
			.into_iter()
			.map(|(k, v)| (k.to_string(), v))
			.collect();
		frontend
			.tasks
			.push(FrontendTask::Lvrr(LvrrRequest::SetHeadsetSettings(Value::Object(
				patch,
			))));
	}

	fn render(&mut self, frontend: &mut Frontend<T>, data: &mut T, generation: u64) -> anyhow::Result<()> {
		self.rendered = Some(generation);
		let headset = frontend.interface.lvrr_state(data).and_then(|s| s.headset.clone());
		let desktop = frontend.interface.lvrr_desktop();

		frontend.layout.remove_children(self.id_content);
		let globals = frontend.layout.state.globals.clone();
		let mut widgets =
			wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, self.id_content)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut widgets, &globals);
		let parent = self.id_content;
		self.sliders.clear();

		let Some(h) = headset else {
			// the server remembers a headset's settings once it connected
			let message = if desktop {
				"LVRR.HEADSET.CONNECT_ONCE"
			} else {
				"LVRR.HEADSET.NOT_CONNECTED"
			};
			ui.banner(parent, message, "secondary")?;
			self.widgets = Some(widgets);
			return Ok(());
		};

		if h.offline {
			ui.banner(parent, "LVRR.HEADSET.OFFLINE", "primary")?;
		} else if h.restart_required {
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
		if !h.offline && !h.permission_denied.is_empty() {
			ui.banner(parent, "LVRR.HEADSET.PERMISSION_DENIED", "danger")?;
		}

		let (title, icon) = self.section.title_icon();
		let group = ui.group(parent, title, icon)?;
		if self.section == Section::System {
			let row = ui.row(group)?;
			ui.badge(row, "LVRR.HEADSET.ADVANCED", "danger")?;
			ui.muted(row, Text::Key("LVRR.HEADSET.ADVANCED_NOTE"))?;
		}
		if self.section == Section::Video {
			mount_presets(&mut ui, group, &h, &self.tasks)?;
		}
		for setting in settings().iter().filter(|s| s.section == self.section && (s.show)(&h)) {
			mount(&mut ui, group, setting, &h, &self.tasks, &mut self.sliders)?;
		}
		if self.section == Section::Devices && !h.server_input_forwarding {
			ui.muted(group, Text::Key("LVRR.HEADSET.NO_INPUT_FORWARDING"))?;
		}

		self.widgets = Some(widgets);
		Ok(())
	}
}

fn mount_presets(ui: &mut Ui, parent: WidgetID, h: &HeadsetSettings, tasks: &Tasks<Task>) -> anyhow::Result<()> {
	let row = ui.row(parent)?;
	let current = PRESETS
		.iter()
		.find(|p| p.matches(h))
		.map_or("LVRR.HEADSET.PRESET_CUSTOM", |p| p.label);
	let cells = PRESETS
		.iter()
		.enumerate()
		.map(|(i, p)| ui::menu_cell(format!("{PRESET_ACTION_PREFIX}{i}"), key(p.label)))
		.collect();
	let tasks = tasks.clone();
	ui.dropdown(
		row,
		"LVRR.HEADSET.PRESET",
		key(current),
		cells,
		move |position, cells| {
			tasks.push(Task::OpenMenu(position, cells));
		},
	)?;
	if !h.offline {
		ui.badge(row, "LVRR.APPLIES_AFTER_RESTART", "tertiary")?;
	}
	Ok(())
}

fn mount(
	ui: &mut Ui,
	parent: WidgetID,
	setting: &Setting,
	h: &HeadsetSettings,
	tasks: &Tasks<Task>,
	sliders: &mut Vec<Rc<ComponentSlider>>,
) -> anyhow::Result<()> {
	let row = ui.row(parent)?;
	let key = setting.key;
	// offline, everything applies when the headset connects
	let badge = if h.offline {
		h.pending.get(key).map(|_| ("LVRR.HEADSET.PENDING", "primary"))
	} else if setting.restart {
		Some(("LVRR.APPLIES_AFTER_RESTART", "tertiary"))
	} else {
		None
	};
	// a slider shows the restart badge next to its own label
	let mut badge_consumed = false;
	match &setting.control {
		Control::Toggle { get, set } => {
			let (tasks, set) = (tasks.clone(), *set);
			ui.toggle(row, Text::Key(setting.label), get(&h.config), move |v| {
				tasks.push(Task::Edit {
					key,
					value: set(v),
					now: true,
				});
			})?;
		}
		Control::Slider { range, get, set } => {
			let (tasks, set) = (tasks.clone(), *set);
			let slider = ui.slider(row, setting.label, badge, range(h), get(&h.config), move |v| {
				tasks.push(Task::Edit {
					key,
					value: set(v),
					now: false,
				});
			})?;
			sliders.push(slider);
			badge_consumed = true;
		}
		Control::Choice { options, get } => {
			let current = get(&h.config);
			let options = options(h);
			let current_text = options.iter().find(|(value, _)| *value == current).map_or_else(
				|| Translation::from_raw_text_string(current.to_string()),
				|(_, t)| t.clone(),
			);
			let cells = options
				.into_iter()
				.map(|(value, title)| ui::menu_cell(format!("{ACTION_PREFIX}{key};{value}"), title))
				.collect();
			let tasks = tasks.clone();
			ui.dropdown(row, setting.label, current_text, cells, move |position, cells| {
				tasks.push(Task::OpenMenu(position, cells));
			})?;
		}
		Control::BodyParts => {
			let column = ui.column(row)?;
			ui.text(column, Text::Key(setting.label))?;
			let selected = Rc::new(RefCell::new(h.config.body_parts.clone()));
			let parts = ui.wrap_row(column)?;
			for part in BODY_PARTS
				.iter()
				.filter(|p| h.capabilities.body_parts.iter().any(|c| c == *p))
			{
				let (tasks, selected) = (tasks.clone(), selected.clone());
				let label = format!("LVRR.HEADSET.BODY.{}", part.to_uppercase());
				let checked = selected.borrow().iter().any(|p| p == part);
				ui.toggle(parts, Text::Key(&label), checked, move |on| {
					let mut list = selected.borrow_mut();
					list.retain(|p| p != part);
					if on {
						list.push((*part).to_string());
					}
					tasks.push(Task::Edit {
						key,
						value: json!(*list),
						now: true,
					});
				})?;
			}
		}
	}
	if setting_advanced(key) {
		ui.badge(row, "LVRR.HEADSET.ADVANCED", "danger")?;
	}
	if !badge_consumed && let Some((translation, color)) = badge {
		ui.badge(row, translation, color)?;
	}
	if let Some(description) = setting_description(key) {
		ui.muted(parent, Text::Key(description))?;
	}
	Ok(())
}

/// Settings that can break the stream or drain the battery, flagged with an Advanced badge
fn setting_advanced(key: &str) -> bool {
	matches!(key, "extended_config" | "high_power_mode")
}

/// A one-line explanation shown under a setting whose name is not self-explanatory
fn setting_description(key: &str) -> Option<&'static str> {
	match key {
		"extended_config" => Some("LVRR.HEADSET.EXTENDED_CONFIG_DESC"),
		"high_power_mode" => Some("LVRR.HEADSET.HIGH_POWER_DESC"),
		_ => None,
	}
}
