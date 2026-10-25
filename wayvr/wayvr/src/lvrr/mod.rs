//! LVRR: the link to the LVRR server, and dashboard features that need the backend.

mod perf_overlay;

use std::io::Write;

use lvrr_common::{
    link::LvrrLink,
    state::{Battery, BatteryDevice, LvrrBackend, MessageKind, Notice},
};
use serde::{Deserialize, Serialize};
use wgui::i18n::Translation;
use wlx_common::{config_io, overlays::ToastTopic};

use crate::{backend::input::TrackedDeviceRole, overlays::toast::Toast, state::AppState};

const CONFIG_FILE: &str = "lvrr.json5";

#[derive(Default)]
pub struct LvrrApp {
    pub link: Option<LvrrLink>,
    pub perf_overlay: bool,
}

/// LVRR's own WayVR settings
#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct LvrrConfig {
    perf_overlay: bool,
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
    app.lvrr.perf_overlay = LvrrConfig::load().perf_overlay;
    if app.lvrr.perf_overlay {
        perf_overlay::set_visible(app, true);
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
    let config = LvrrConfig {
        perf_overlay: visible,
    };
    if let Err(e) = config.save() {
        log::warn!("Could not save {CONFIG_FILE}: {e:?}");
    }
}

/// Shows the server's messages, and failed requests, as toasts
pub fn tick(app: &mut AppState) {
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
