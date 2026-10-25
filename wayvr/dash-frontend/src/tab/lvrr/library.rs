//! VR applications the LVRR server can start: Steam VR games and desktop entries in the
//! X-WiVRn-VR category. They start with the server's environment, so OpenVR games use the
//! bundled xrizer.

use std::marker::PhantomData;

use lvrr_common::state::LvrrRequest;
use wgui::{layout::WidgetID, parser::ParserState, task::Tasks};

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

/// The dashboard itself is listed as a VR app when WayVR is installed system-wide
const HIDDEN_APPS: &[&str] = &["wayvr"];

#[derive(Clone)]
enum Task {
	Request(LvrrRequest),
	Frontend(FrontendTask),
}

pub struct TabLvrrLibrary<T> {
	#[allow(dead_code)]
	state: ParserState,
	widgets: Option<ParserState>,
	tasks: Tasks<Task>,
	parent: WidgetID,
	rendered: Option<u64>,
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
			}
		}
		let generation = frontend.interface.lvrr_state(data).map_or(0, |s| s.generation);
		if self.rendered != Some(generation) {
			self.render(frontend, data, generation)?;
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
			rendered: None,
			marker: PhantomData,
		})
	}

	fn render(&mut self, frontend: &mut Frontend<T>, data: &mut T, generation: u64) -> anyhow::Result<()> {
		self.rendered = Some(generation);
		let applications = frontend.interface.lvrr_state(data).and_then(|s| s.applications.clone());

		frontend.layout.remove_children(self.parent);
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

		match applications {
			None => ui.muted(group, Text::Key("LVRR.LIBRARY.LOADING"))?,
			Some(applications) => {
				let applications: Vec<_> = applications
					.into_iter()
					.filter(|a| !HIDDEN_APPS.contains(&a.id.as_str()))
					.collect();
				if applications.is_empty() {
					ui.muted(group, Text::Key("LVRR.LIBRARY.EMPTY"))?;
				}
				for app in applications {
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
					ui.bold(row, Text::Raw(&app.name), 16)?;
				}
			}
		}

		self.widgets = Some(widgets);
		Ok(())
	}
}
