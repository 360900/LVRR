//! The controller a game sees in place of the real one, chosen per game, like Virtual Desktop's
//! controller emulation: `XRIZER_CONTROLLER_TYPE`, else `controller_type` in
//! `$XDG_CONFIG_HOME/xrizer/apps/<app key>.json`, for each key the app may have.
//!
//! The game's bindings for that controller are converted onto the real one, which then reports
//! that controller's identity and poses. The headset keeps its own.
//!
//! Without a choice, a controller the game has no bindings for shows as the one its bindings are
//! converted from, if that one can be shown.

use super::action_manifest::{ControllerType, env_var, isolated_env_var};
use super::binding_sources::{key_dir_name, xrizer_config_dir};
use super::steamvr_bindings;

/// The controllers a game can be shown instead of the real ones.
const EMULATED_TYPES: [ControllerType; 2] = [ControllerType::OculusTouch, ControllerType::Knuckles];

impl ControllerType {
    /// Whether a game can be shown this controller instead of the real one
    pub fn can_be_emulated(&self) -> bool {
        EMULATED_TYPES.contains(self)
    }
}

/// The controller the app that identified as `app_key` should see, if one was chosen for it.
pub fn emulated_controller(app_key: Option<String>) -> Option<ControllerType> {
    let chosen = env_var("XRIZER_CONTROLLER_TYPE")
        .and_then(|value| value.into_string().ok())
        .or_else(|| {
            let dir = xrizer_config_dir()?.join("apps");
            steamvr_bindings::app_keys(&isolated_env_var, app_key)
                .iter()
                .filter_map(|key| key_dir_name(key))
                .find_map(|name| {
                    let data = std::fs::read(dir.join(format!("{name}.json"))).ok()?;
                    let file: serde_json::Value = serde_json::from_slice(&data).ok()?;
                    Some(file.get("controller_type")?.as_str()?.to_owned())
                })
        })?;
    let controller_type = chosen.parse().unwrap();
    if EMULATED_TYPES.contains(&controller_type) {
        Some(controller_type)
    } else {
        // "auto", or something else
        None
    }
}
