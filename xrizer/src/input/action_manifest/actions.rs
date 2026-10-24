#![allow(private_interfaces)]

use super::bindings::{ActionPath, lenient_vec};
use crate::openxr_data::Hand;
use log::{error, trace, warn};
use openxr as xr;
use serde::{
    Deserialize,
    de::{Error, Unexpected},
};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/**
 * Structure for action manifests.
 * https://github.com/ValveSoftware/openvr/wiki/Action-manifest
 */

#[derive(Deserialize)]
pub struct ActionManifest {
    #[serde(default, deserialize_with = "lenient_vec")]
    pub default_bindings: Vec<DefaultBindings>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub action_sets: Vec<ActionSetJson>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub actions: Vec<ActionType>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub localization: Vec<Localization>,
    // localization_files
}

#[derive(Deserialize, Debug, Clone)]
pub struct DefaultBindings {
    pub binding_url: PathBuf,
    pub controller_type: ControllerType,
}

#[derive(Deserialize, Debug, PartialEq, Eq, Hash, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ControllerType {
    ViveController,
    #[serde(rename = "vive_focus3_controller")]
    ViveFocus3,
    Knuckles,
    FrameController,
    OculusTouch,
    HolographicController,
    #[serde(rename = "hpmotioncontroller")]
    HPMotionController,
    /// The headset, see [super::bindings::handle_hmd_bindings].
    GenericHmd,
    #[serde(untagged)]
    Unknown(String),
}

impl std::str::FromStr for ControllerType {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "vive_controller" => Self::ViveController,
            "vive_focus3_controller" => Self::ViveFocus3,
            "knuckles" => Self::Knuckles,
            "frame_controller" => Self::FrameController,
            "oculus_touch" => Self::OculusTouch,
            "holographic_controller" => Self::HolographicController,
            "hpmotioncontroller" => Self::HPMotionController,
            "generic_hmd" => Self::GenericHmd,
            other => Self::Unknown(other.to_owned()),
        })
    }
}

impl std::fmt::Display for ControllerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::ViveController => "vive_controller",
            Self::ViveFocus3 => "vive_focus3_controller",
            Self::Knuckles => "knuckles",
            Self::FrameController => "frame_controller",
            Self::OculusTouch => "oculus_touch",
            Self::HolographicController => "holographic_controller",
            Self::HPMotionController => "hpmotioncontroller",
            Self::GenericHmd => "generic_hmd",
            Self::Unknown(value) => value,
        };
        f.write_str(value)
    }
}

#[derive(Deserialize)]
pub struct ActionSetJson {
    #[serde(rename = "name")]
    path: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum ActionType {
    Boolean(ActionDataCommon),
    Vector1(ActionDataCommon),
    Vector2(ActionDataCommon),
    Vector3(ActionDataCommon),
    Vibration(ActionDataCommon),
    Pose(ActionDataCommon),
    Skeleton(SkeletonData),
}

#[derive(Deserialize)]
struct ActionDataCommon {
    name: ActionPath,
}

#[derive(Deserialize)]
struct SkeletonData {
    #[serde(deserialize_with = "parse_skeleton")]
    skeleton: Hand,
    #[serde(flatten)]
    data: ActionDataCommon,
}

fn parse_skeleton<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Hand, D::Error> {
    let path: &str = Deserialize::deserialize(d)?;
    let Some(hand) = path.strip_prefix("/skeleton/hand") else {
        return Err(D::Error::invalid_value(
            Unexpected::Str(path),
            &"path starting with /skeleton/hand",
        ));
    };

    match hand {
        "/left" => Ok(Hand::Left),
        "/right" => Ok(Hand::Right),
        _ => Err(D::Error::invalid_value(
            Unexpected::Str(hand),
            &r#""/left" or "/right""#,
        )),
    }
}

#[derive(Deserialize)]
pub struct Localization {
    pub language_tag: String,
    #[serde(flatten)]
    localized_names: HashMap<String, serde_json::Value>,
}

impl Localization {
    /// Localized names keyed by lowercase path, like action and set paths.
    pub fn into_names(self) -> HashMap<String, String> {
        self.localized_names
            .into_iter()
            .filter_map(|(path, name)| Some((path.to_lowercase(), name.as_str()?.to_owned())))
            .collect()
    }
}

/// Replaces every character OpenXR doesn't allow in action (set) names.
pub fn xr_friendly(name: &str) -> String {
    name.replace(
        |c| !matches!(c, 'a'..='z' | '0'..='9' | '-' | '_' | '.'),
        "_",
    )
}

/// Cuts `s` to at most `max` bytes, on a character boundary.
fn truncate(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// OpenXR names handed out so far. They must be unique (per instance for sets, per set for
/// actions) and fit the runtime's buffers, while OpenVR paths may be any length and different
/// paths can clean to the same name.
#[derive(Default)]
pub struct XrNames {
    sets: HashSet<String>,
    actions: HashMap<String, HashSet<String>>,
}

impl XrNames {
    fn set_name(&mut self, base: &str) -> String {
        Self::unique(&mut self.sets, base, xr::sys::MAX_ACTION_SET_NAME_SIZE)
    }

    pub fn action_name(&mut self, set_path: &str, base: &str) -> String {
        Self::unique(
            self.actions.entry(set_path.to_owned()).or_default(),
            base,
            xr::sys::MAX_ACTION_NAME_SIZE,
        )
    }

    fn unique(used: &mut HashSet<String>, base: &str, size: usize) -> String {
        let base = if base.is_empty() { "unnamed" } else { base };
        // The buffer includes the NUL terminator.
        let max = size - 1;
        let mut name = truncate(base, max).to_owned();
        let mut idx = 0;
        while used.contains(&name) {
            idx += 1;
            let suffix = format!("_{idx}");
            name = [truncate(base, max - suffix.len()), &suffix].concat();
        }
        used.insert(name.clone());
        name
    }
}

/// Creates an OpenXR action or action set with a localized name that fits `size` (including the
/// NUL terminator), making it unique with the (unique) name if the runtime says it isn't.
fn create_localized<T>(
    localized: Option<&str>,
    name: &str,
    size: usize,
    create: impl Fn(&str) -> xr::Result<T>,
) -> xr::Result<T> {
    let localized = truncate(
        localized.filter(|l| !l.is_empty()).unwrap_or(name),
        size - 1,
    );
    create(localized).or_else(|err| {
        if err != xr::sys::Result::ERROR_LOCALIZED_NAME_DUPLICATED {
            return Err(err);
        }
        let suffix = format!(" ({name})");
        create(&[truncate(localized, size - 1 - suffix.len()), &suffix].concat())
    })
}

fn create_action_set(
    instance: &xr::Instance,
    names: &mut XrNames,
    path: &str,
    localized: Option<&str>,
) -> Option<xr::ActionSet> {
    // OpenXR does not like the "/actions/<set name>" format, so we need to strip the prefix
    let Some(name) = path.strip_prefix("/actions/") else {
        error!("Action set {path} missing actions prefix.");
        return None;
    };
    let xr_friendly_name = names.set_name(&xr_friendly(name));

    trace!("Creating action set {xr_friendly_name} ({path:?}) (localized: {localized:?})");
    create_localized(
        Some(localized.unwrap_or(path)),
        &xr_friendly_name,
        xr::sys::MAX_LOCALIZED_ACTION_SET_NAME_SIZE,
        |localized| instance.create_action_set(&xr_friendly_name, localized, 0),
    )
    .inspect_err(|e| error!("Failed to create action set {xr_friendly_name}: {e}"))
    .ok()
}

pub fn load_action_sets(
    instance: &xr::Instance,
    names: &mut XrNames,
    english: &HashMap<String, String>,
    sets: Vec<ActionSetJson>,
) -> HashMap<String, xr::ActionSet> {
    let mut action_sets = HashMap::new();
    for ActionSetJson { path } in sets {
        let path = path.to_lowercase();
        if action_sets.contains_key(&path) {
            warn!("Action set {path} is listed twice, ignoring the second one");
            continue;
        }
        let localized = english.get(&path).map(String::as_str);
        // Actions in a set that couldn't be created are skipped.
        if let Some(set) = create_action_set(instance, names, &path, localized) {
            action_sets.insert(path, set);
        }
    }
    action_sets
}

fn create_action<T: xr::ActionTy>(
    instance: &xr::Instance,
    names: &mut XrNames,
    data: &ActionDataCommon,
    sets: &mut HashMap<String, xr::ActionSet>,
    english: &HashMap<String, String>,
    paths: &[xr::Path],
) -> Option<xr::Action<T>> {
    let Some(set_name) = data.name.action_set_name() else {
        warn!(
            "Action {} is not in an action set, skipping",
            data.name.path
        );
        return None;
    };
    let set = match sets.entry(set_name.to_owned()) {
        Entry::Occupied(set) => set.into_mut(),
        Entry::Vacant(entry) => {
            warn!("Action set {set_name} is missing from manifest, creating it...");
            entry.insert(create_action_set(instance, names, set_name, None)?)
        }
    };
    let xr_friendly_name = names.action_name(set_name, &data.name.cleaned_name());
    let localized = english.get(&data.name.path).map(String::as_str);
    trace!("Creating action {xr_friendly_name} (localized: {localized:?}) in set {set_name:?}");

    create_localized(
        localized,
        &xr_friendly_name,
        xr::sys::MAX_LOCALIZED_ACTION_NAME_SIZE,
        |localized| set.create_action(&xr_friendly_name, localized, paths),
    )
    .inspect_err(|e| {
        warn!(
            "Failed to create action {} ({xr_friendly_name}), skipping: {e}",
            data.name.path
        )
    })
    .ok()
}

pub type LoadedActionDataMap = HashMap<String, crate::input::ActionData>;
pub fn load_actions(
    instance: &xr::Instance,
    names: &mut XrNames,
    english: &HashMap<String, String>,
    sets: &mut HashMap<String, xr::ActionSet>,
    actions: Vec<ActionType>,
    left_hand: xr::Path,
    right_hand: xr::Path,
) -> LoadedActionDataMap {
    let mut ret = HashMap::with_capacity(actions.len());
    for action in actions {
        let paths = &[left_hand, right_hand];
        macro_rules! create_action {
            ($ty:ty, $data:expr) => {{
                let Some(action) =
                    create_action::<$ty>(instance, names, &$data, sets, english, paths)
                else {
                    continue;
                };
                action
            }};
        }
        let name = match &action {
            ActionType::Boolean(data)
            | ActionType::Vector1(data)
            | ActionType::Vector2(data)
            | ActionType::Vector3(data)
            | ActionType::Vibration(data)
            | ActionType::Pose(data)
            | ActionType::Skeleton(SkeletonData { data, .. }) => &data.name.path,
        };
        if ret.contains_key(name) {
            warn!("Action {name} is listed twice, ignoring the second one");
            continue;
        }
        use crate::input::ActionData::*;
        let (path, action) = match &action {
            ActionType::Boolean(data) => (&data.name, Bool(create_action!(bool, data))),
            ActionType::Vector1(data) => (
                &data.name,
                Vector1 {
                    action: create_action!(f32, data),
                    state: Default::default(),
                    input_only: false,
                },
            ),
            ActionType::Vector2(data) => (
                &data.name,
                Vector2 {
                    action: create_action!(xr::Vector2f, data),
                    data: Default::default(),
                },
            ),
            ActionType::Vector3(data) => {
                warn!(
                    "Got vector3 action {}, but these are currently unsupported.",
                    data.name.path
                );
                continue;
            }
            ActionType::Pose(data) => (&data.name, Pose),
            ActionType::Skeleton(SkeletonData { skeleton, data }) => {
                trace!("Creating skeleton action {}", data.name.path);
                (&data.name, Skeleton(*skeleton))
            }
            ActionType::Vibration(data) => (&data.name, Haptic(create_action!(xr::Haptic, data))),
        };
        ret.insert(path.path.clone(), action);
    }
    ret
}
