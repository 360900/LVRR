//! VR applications the LVRR server can start: Steam VR games and desktop entries in the
//! X-WiVRn-VR category. They start with the server's environment, so OpenVR games use the
//! bundled xrizer.

use std::{collections::HashMap, marker::PhantomData, path::Path, rc::Rc};

use lvrr_common::state::{LvrrRequest, VrApplication};
use wgui::{layout::WidgetID, parser::ParserState, task::Tasks, widget::ConstructEssentials};

use crate::{
	frontend::{Frontend, FrontendTask},
	tab::{
		Tab, TabType,
		lvrr::{
			Page,
			ui::{self, Text, Ui},
		},
	},
	util::{cached_fetcher::CoverArt, steam_utils::AppManifest},
	views::game_cover,
};

/// The dashboard itself is listed as a VR app when WayVR is installed system-wide
const HIDDEN_APPS: &[&str] = &["wayvr"];

/// How often to look for the started app's session
const POLL_MS: u32 = 500;
/// Steam can take a while to update or start a game, after this the notice goes away
const STARTING_TIMEOUT_MS: u32 = 180_000;

#[derive(Clone)]
enum Task {
	Request(LvrrRequest),
	Frontend(FrontendTask),
	SetCover(String, Rc<CoverArt>),
	Start { id: String, name: String },
}

/// An app that was started and has no session yet
struct Starting {
	name: String,
	/// Sessions that existed before the start
	known: Vec<i64>,
	since_ms: u32,
	polled_ms: u32,
}

pub struct TabLvrrLibrary<T> {
	#[allow(dead_code)]
	state: ParserState,
	widgets: Option<ParserState>,
	tasks: Tasks<Task>,
	parent: WidgetID,
	/// The inner None until the server answered
	shown: Option<Option<Vec<VrApplication>>>,
	cover_common: game_cover::ViewCommon,
	covers: HashMap<String, game_cover::View>,
	starting: Option<Starting>,
	marker: PhantomData<T>,
}

impl<T: 'static> Tab<T> for TabLvrrLibrary<T> {
	fn get_type(&self) -> TabType {
		TabType::Lvrr(Page::Library)
	}

	fn update(&mut self, frontend: &mut Frontend<T>, time_ms: u32, data: &mut T) -> anyhow::Result<()> {
		let mut redraw = false;
		for task in self.tasks.drain() {
			match task {
				Task::Request(request) => frontend.tasks.push(FrontendTask::Lvrr(request)),
				Task::Frontend(task) => frontend.tasks.push(task),
				Task::SetCover(id, cover) => {
					if let Some(view) = self.covers.get_mut(&id) {
						view.set_cover_art(&mut self.cover_common, &mut frontend.layout, &cover)?;
					}
				}
				Task::Start { id, name } => {
					let known = session_ids(frontend, data);
					self.starting = Some(Starting {
						name,
						known,
						since_ms: time_ms,
						polled_ms: time_ms,
					});
					frontend
						.tasks
						.push(FrontendTask::Lvrr(LvrrRequest::StartApplication(id)));
					redraw = true;
				}
			}
		}
		if let Some(starting) = &mut self.starting
			&& time_ms.wrapping_sub(starting.polled_ms) >= POLL_MS
		{
			starting.polled_ms = time_ms;
			let appeared = session_ids(frontend, data)
				.iter()
				.any(|id| !starting.known.contains(id));
			if appeared || time_ms.wrapping_sub(starting.since_ms) >= STARTING_TIMEOUT_MS {
				self.starting = None;
				redraw = true;
			}
		}
		if redraw {
			self.shown = None;
		}
		let applications = frontend.interface.lvrr_state(data).and_then(|s| s.applications.clone());
		let applications = applications.map(|apps| {
			apps
				.into_iter()
				.filter(|a| !HIDDEN_APPS.contains(&a.id.as_str()))
				.collect::<Vec<_>>()
		});
		if self.shown.as_ref() != Some(&applications) {
			self.render(frontend, applications.as_deref())?;
			self.shown = Some(applications);
		}
		Ok(())
	}
}

impl<T: 'static> TabLvrrLibrary<T> {
	pub fn new(frontend: &mut Frontend<T>, parent: WidgetID, _data: &mut T) -> anyhow::Result<Self> {
		let globals = frontend.layout.state.globals.clone();
		let state = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, parent)?;
		let tasks = Tasks::new();
		// the list may have changed since the last visit
		tasks.push(Task::Request(LvrrRequest::RefreshApplications));
		Ok(Self {
			state,
			widgets: None,
			tasks,
			parent,
			shown: None,
			cover_common: game_cover::ViewCommon::new(globals),
			covers: HashMap::new(),
			starting: None,
			marker: PhantomData,
		})
	}

	fn render(&mut self, frontend: &mut Frontend<T>, applications: Option<&[VrApplication]>) -> anyhow::Result<()> {
		frontend.layout.remove_children(self.parent);
		self.covers.clear();
		let globals = frontend.layout.state.globals.clone();
		let mut widgets = wgui::parser::parse_from_assets(&ui::doc_params(&globals), &mut frontend.layout, self.parent)?;
		let mut ui = Ui::new(&mut frontend.layout, &mut widgets, &globals);

		let group = ui.group(self.parent, "LVRR.LIBRARY.VR_APPS", "@/dashboard/games.svg")?;
		let row = ui.row(group)?;
		let tasks = self.tasks.clone();
		ui.button(
			row,
			Text::Key("LVRR.LIBRARY.REFRESH"),
			"@/dashboard/refresh.svg",
			move || {
				tasks.push(Task::Request(LvrrRequest::RefreshApplications));
			},
		)?;
		let tasks = self.tasks.clone();
		ui.button(
			row,
			Text::Key("LVRR.LIBRARY.ALL_GAMES"),
			"@/dashboard/games.svg",
			move || {
				tasks.push(Task::Frontend(FrontendTask::SetTab(TabType::Games)));
			},
		)?;

		if let Some(starting) = &self.starting {
			let row = ui.row(group)?;
			ui.text(row, Text::Key("LVRR.LIBRARY.STARTING"))?;
			ui.bold(row, Text::Raw(&starting.name), 16)?;
		}

		let Some(applications) = applications else {
			ui.muted(group, Text::Key("LVRR.LIBRARY.LOADING"))?;
			self.widgets = Some(widgets);
			return Ok(());
		};
		if applications.is_empty() {
			ui.muted(group, Text::Key("LVRR.LIBRARY.EMPTY"))?;
		}
		let (steam, other): (Vec<_>, Vec<_>) = applications.iter().partition(|a| a.steam_app_id().is_some());

		// Steam games as covers, like the Games tab
		if !steam.is_empty() {
			let covers = ui.row(group)?;
			for app in steam {
				let manifest = AppManifest {
					app_id: app.id.clone(),
					run_game_id: app.id.clone(),
					name: app.name.clone(),
					raw_state_flags: 0,
					last_played: None,
				};
				let on_loaded = {
					let (tasks, id) = (self.tasks.clone(), app.id.clone());
					Box::new(move |cover: CoverArt| tasks.push(Task::SetCover(id, Rc::new(cover))))
				};
				let view = game_cover::View::new(game_cover::Params {
					ess: &mut ConstructEssentials {
						layout: ui.layout,
						parent: covers,
					},
					executor: &frontend.executor,
					manifest: &manifest,
					scale: 1.0,
					on_loaded,
				})?;
				let (tasks, id, name) = (self.tasks.clone(), app.id.clone(), app.name.clone());
				view.button.on_click(Rc::new(move |_, _| {
					tasks.push(Task::Start {
						id: id.clone(),
						name: name.clone(),
					});
					Ok(())
				}));
				self.covers.insert(app.id.clone(), view);
			}
		}

		// Desktop entries with their icon
		for app in other {
			let row = ui.row(group)?;
			let (tasks, id, name) = (self.tasks.clone(), app.id.clone(), app.name.clone());
			ui.button(
				row,
				Text::Key("LVRR.LIBRARY.LAUNCH"),
				"@/dashboard/play.svg",
				move || {
					tasks.push(Task::Start {
						id: id.clone(),
						name: name.clone(),
					});
				},
			)?;
			if let Some(icon) = icon_path(&app.icon)
				&& let Err(e) = ui.file_icon(row, icon)
			{
				log::warn!("Could not show the icon of {}: {e:?}", app.id);
			}
			ui.bold(row, Text::Raw(&app.name), 16)?;
		}

		self.widgets = Some(widgets);
		Ok(())
	}
}

/// VR sessions other than overlays such as the dashboard
fn session_ids<T>(frontend: &mut Frontend<T>, data: &mut T) -> Vec<i64> {
	let clients = frontend.interface.monado_client_list(data, true).unwrap_or_default();
	clients.iter().map(|c| c.id).collect()
}

/// An icon file the dashboard can draw
fn icon_path(icon: &str) -> Option<&str> {
	let path = Path::new(icon);
	let drawable = path
		.extension()
		.is_some_and(|e| e.eq_ignore_ascii_case("png") || e.eq_ignore_ascii_case("svg"));
	(path.is_absolute() && drawable && path.is_file()).then_some(icon)
}
