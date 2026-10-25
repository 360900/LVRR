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

#[derive(Clone)]
enum Task {
	Request(LvrrRequest),
	Frontend(FrontendTask),
	SetCover(String, Rc<CoverArt>),
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
	marker: PhantomData<T>,
}

impl<T: 'static> Tab<T> for TabLvrrLibrary<T> {
	fn get_type(&self) -> TabType {
		TabType::Lvrr(Page::Library)
	}

	fn update(&mut self, frontend: &mut Frontend<T>, _time_ms: u32, data: &mut T) -> anyhow::Result<()> {
		for task in self.tasks.drain() {
			match task {
				Task::Request(request) => frontend.tasks.push(FrontendTask::Lvrr(request)),
				Task::Frontend(task) => frontend.tasks.push(task),
				Task::SetCover(id, cover) => {
					if let Some(view) = self.covers.get_mut(&id) {
						view.set_cover_art(&mut self.cover_common, &mut frontend.layout, &cover)?;
					}
				}
			}
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
				let (tasks, id) = (self.tasks.clone(), app.id.clone());
				view.button.on_click(Rc::new(move |_, _| {
					tasks.push(Task::Request(LvrrRequest::StartApplication(id.clone())));
					Ok(())
				}));
				self.covers.insert(app.id.clone(), view);
			}
		}

		// Desktop entries with their icon
		for app in other {
			let row = ui.row(group)?;
			let (tasks, id) = (self.tasks.clone(), app.id.clone());
			ui.button(
				row,
				Text::Key("LVRR.LIBRARY.LAUNCH"),
				"@/dashboard/play.svg",
				move || {
					tasks.push(Task::Request(LvrrRequest::StartApplication(id.clone())));
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

/// An icon file the dashboard can draw
fn icon_path(icon: &str) -> Option<&str> {
	let path = Path::new(icon);
	let drawable = path
		.extension()
		.is_some_and(|e| e.eq_ignore_ascii_case("png") || e.eq_ignore_ascii_case("svg"));
	(path.is_absolute() && drawable && path.is_file()).then_some(icon)
}
