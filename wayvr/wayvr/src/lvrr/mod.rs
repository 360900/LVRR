//! LVRR: the link to the LVRR server, and dashboard features that need the backend.

mod perf_overlay;

use std::{
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};

use lvrr_common::{
    link::LvrrLink,
    state::{Battery, BatteryDevice, LvrrBackend, MAX_VIRTUAL_MONITORS, MessageKind, Notice},
};
use serde::{Deserialize, Serialize};
use wgui::i18n::Translation;
use wlx_common::{config_io, overlays::ToastTopic};

#[cfg(feature = "wayland")]
use crate::overlays::screen::virtual_monitor;
use crate::{
    backend::{
        input::TrackedDeviceRole,
        task::{OverlayTask, TaskType},
    },
    overlays::toast::Toast,
    state::AppState,
    windowing::OverlaySelector,
};

const CONFIG_FILE: &str = "lvrr.json5";

#[derive(Default)]
pub struct LvrrApp {
    pub link: Option<LvrrLink>,
    pub perf_overlay: bool,
    /// The dashboard has a focused edit box, the keyboard types into it
    pub dash_text_focus: bool,
    /// Typed text for the dashboard
    pub dash_text: Vec<String>,
    pub virtual_monitors: Vec<VirtualMonitor>,
    /// How this desktop adds outputs, once looked up; None when it can't
    #[cfg(feature = "wayland")]
    virtual_method: std::cell::OnceCell<Option<virtual_monitor::Method>>,
    /// The mouse's area changes once a removed output is gone
    extent_refresh_at: Option<Instant>,
}

pub struct VirtualMonitor {
    pub name: Arc<str>,
    pub width: u32,
    pub height: u32,
    /// The compositor's output, once found
    pub output: Option<Arc<str>>,
}

/// LVRR's own WayVR settings
#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct LvrrConfig {
    perf_overlay: bool,
    /// Sizes of the virtual monitors, added again at start
    virtual_monitors: Vec<(u32, u32)>,
}

impl LvrrConfig {
    fn load() -> Self {
        let path = config_io::get_config_file_path(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(json) => serde_json5::from_str(&json).unwrap_or_else(|e| {
                log::warn!("Ignoring {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    fn save(&self) -> anyhow::Result<()> {
        let path = config_io::get_config_file_path(CONFIG_FILE);
        std::fs::write(path, serde_json5::to_string(self)?)?;
        Ok(())
    }
}

pub fn start(app: &mut AppState) {
    if app.lvrr.link.is_some() {
        return;
    }
    match LvrrLink::start() {
        Ok(link) => app.lvrr.link = Some(link),
        Err(e) => log::error!("Could not start the LVRR server link: {e:?}"),
    }
    let config = LvrrConfig::load();
    app.lvrr.perf_overlay = config.perf_overlay;
    if app.lvrr.perf_overlay {
        perf_overlay::set_visible(app, true);
    }
    for (width, height) in config.virtual_monitors {
        add_virtual_monitor(app, width, height);
    }
}

fn save_config(app: &AppState) {
    let config = LvrrConfig {
        perf_overlay: app.lvrr.perf_overlay,
        virtual_monitors: app
            .lvrr
            .virtual_monitors
            .iter()
            .map(|m| (m.width, m.height))
            .collect(),
    };
    if let Err(e) = config.save() {
        log::warn!("Could not save {CONFIG_FILE}: {e:?}");
    }
}

/// Batteries of the headset, controllers and trackers
pub fn batteries(app: &AppState) -> Vec<Battery> {
    app.input_state
        .devices
        .iter()
        .filter_map(|d| {
            let device = match d.role {
                TrackedDeviceRole::Hmd => BatteryDevice::Headset,
                TrackedDeviceRole::LeftHand => BatteryDevice::LeftController,
                TrackedDeviceRole::RightHand => BatteryDevice::RightController,
                TrackedDeviceRole::Tracker => BatteryDevice::Tracker,
                TrackedDeviceRole::None => return None,
            };
            Some(Battery {
                device,
                level: d.soc?,
                charging: d.charging,
            })
        })
        .collect()
}

/// Shows or hides the performance overlay, and remembers the choice
pub fn set_perf_overlay(app: &mut AppState, visible: bool) {
    if app.lvrr.perf_overlay == visible {
        return;
    }
    app.lvrr.perf_overlay = visible;
    perf_overlay::set_visible(app, visible);
    save_config(app);
}

/// Virtual monitors work on this desktop
pub fn virtual_monitors_supported(app: &mut AppState) -> bool {
    #[cfg(feature = "wayland")]
    {
        virtual_method(app).is_some()
    }
    #[cfg(not(feature = "wayland"))]
    {
        let _ = app;
        false
    }
}

#[cfg(feature = "wayland")]
fn virtual_method(app: &mut AppState) -> Option<virtual_monitor::Method> {
    if !app.feats.desktop_backend.is_wayland() {
        return None;
    }
    *app.lvrr.virtual_method.get_or_init(|| {
        let method = virtual_monitor::Method::detect();
        log::info!("Virtual monitors: {method:?}");
        method
    })
}

/// Adds a virtual monitor, shown as a screen, and remembers it
pub fn add_virtual_monitor(app: &mut AppState, width: u32, height: u32) {
    #[cfg(feature = "wayland")]
    {
        let Some(method) = virtual_method(app) else {
            return;
        };
        if app.lvrr.virtual_monitors.len() >= MAX_VIRTUAL_MONITORS {
            return;
        }
        // LVRR-1 to LVRR-3, reusing removed ones
        let Some(name) = (1..=MAX_VIRTUAL_MONITORS)
            .map(|i| Arc::<str>::from(format!("LVRR-{i}")))
            .find(|n| app.lvrr.virtual_monitors.iter().all(|m| m.name != *n))
        else {
            return;
        };
        log::info!("{name}: adding a {width}x{height} virtual monitor");
        app.lvrr.virtual_monitors.push(VirtualMonitor {
            name: name.clone(),
            width,
            height,
            output: None,
        });
        virtual_monitor::spawn(app, method, name, width, height);
        save_config(app);
    }
    #[cfg(not(feature = "wayland"))]
    let _ = (app, width, height);
}

/// Removes a virtual monitor and its screen
pub fn remove_virtual_monitor(app: &mut AppState, name: &str) {
    let before = app.lvrr.virtual_monitors.len();
    app.lvrr.virtual_monitors.retain(|m| &*m.name != name);
    if app.lvrr.virtual_monitors.len() == before {
        return;
    }
    app.tasks
        .enqueue(TaskType::Overlay(OverlayTask::DropImmediate(
            OverlaySelector::Name(name.into()),
        )));
    app.lvrr.extent_refresh_at = Some(Instant::now() + Duration::from_secs(1));
    save_config(app);
}

/// The compositor outputs that belong to virtual monitors
pub fn virtual_monitor_outputs(app: &AppState) -> Vec<Arc<str>> {
    app.lvrr
        .virtual_monitors
        .iter()
        .filter_map(|m| m.output.clone())
        .collect()
}

pub fn set_virtual_monitor_output(app: &mut AppState, name: &str, output: Arc<str>) {
    if let Some(m) = app
        .lvrr
        .virtual_monitors
        .iter_mut()
        .find(|m| &*m.name == name)
    {
        m.output = Some(output);
    }
}

/// Shows the server's messages, and failed requests, as toasts
pub fn tick(app: &mut AppState) {
    #[cfg(feature = "wayland")]
    if app
        .lvrr
        .extent_refresh_at
        .is_some_and(|t| Instant::now() >= t)
    {
        app.lvrr.extent_refresh_at = None;
        if let Some(wl) = wlx_capture::wayland::WlxClient::new() {
            virtual_monitor::set_desktop_extent(app, &wl);
        }
    }

    let notices = match app.lvrr.link.as_mut() {
        Some(link) => link.poll(),
        None => return,
    };
    for notice in notices {
        let (topic, text) = match notice {
            Notice::Message(message) => {
                let topic = match message.kind {
                    MessageKind::Info => ToastTopic::System,
                    MessageKind::Urgent => ToastTopic::Priority,
                    MessageKind::Error => ToastTopic::Error,
                };
                (topic, message.text)
            }
            Notice::RequestFailed { request, error } => {
                (ToastTopic::Error, format!("{request} failed: {error}"))
            }
        };
        Toast::new(
            topic,
            Some(Translation::from_raw_text("LVRR")),
            Translation::from_raw_text_string(text),
        )
        .with_timeout(5.0)
        .submit(app);
    }
}

/// Copies text to the desktop clipboard
pub fn clipboard_set_text(text: &str) -> anyhow::Result<()> {
    for (program, args) in [
        ("wl-copy", &[][..]),
        ("xclip", &["-selection", "clipboard"][..]),
    ] {
        let Ok(mut child) = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        child.wait()?;
        return Ok(());
    }
    anyhow::bail!("Install wl-clipboard or xclip to copy text")
}
