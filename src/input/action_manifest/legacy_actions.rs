//! An equivalent of SteamVR's legacy action manifest (resources/config/legacy_actions.json in
//! SteamVR), which apps without an action manifest go through there, so users can choose bindings
//! for them like for any app.

use super::actions::{self, ControllerType};
use super::bindings::Bindings;
use super::{BindingsLoadContext, read_bindings};
use crate::input::binding_sources::{BindingChoices, BindingSources};
use crate::input::legacy::{LegacyActionData, RemappedLegacy};
use crate::input::profiles::RunWithProfile;
use crate::input::steamvr_bindings::Category;
use crate::input::{Input, InteractionProfile, ManifestLoadedActions};
use crate::openxr_data::{self, Hand, SessionData};
use log::{info, warn};
use openvr as vr;
use openxr as xr;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

/// The set with an action of each kind per hand.
const LEGACY_SET: &str = "/actions/legacy";
/// The set used instead with the mirror_actions option, whose actions both hands drive.
const MIRRORED_SET: &str = "/actions/legacy_mirrored";

/// What an action of the legacy manifest drives in a controller's state.
#[derive(Clone, Copy, Debug)]
pub(in crate::input) enum LegacyOutput {
    Press(vr::EVRButtonId),
    Touch(vr::EVRButtonId),
    /// Both components of an axis, from a vector2 action.
    Axis(usize),
    /// The x component of an axis, from a vector1 action.
    AxisX(usize),
    /// The y component of an axis, from a vector1 action.
    AxisY(usize),
    Pose,
    Haptic,
}

use LegacyOutput::*;
use vr::EVRButtonId as Button;

/// The actions of a hand, as `(name, type, output)`. They are `/actions/legacy/in/Left_<name>` and
/// `/actions/legacy/in/Right_<name>`, and `/actions/legacy_mirrored/in/<name>` for both hands
/// (`out` for the haptic). Bindings name them in any case. Later outputs win over earlier ones
/// of the same axis.
pub(in crate::input) const HAND_ACTIONS: [(&str, &str, LegacyOutput); 28] = [
    ("System_Press", "boolean", Press(Button::System)),
    ("System_Touch", "boolean", Touch(Button::System)),
    (
        "ApplicationMenu_Press",
        "boolean",
        Press(Button::ApplicationMenu),
    ),
    (
        "ApplicationMenu_Touch",
        "boolean",
        Touch(Button::ApplicationMenu),
    ),
    ("Grip_Press", "boolean", Press(Button::Grip)),
    ("Grip_Touch", "boolean", Touch(Button::Grip)),
    ("A_Press", "boolean", Press(Button::A)),
    ("A_Touch", "boolean", Touch(Button::A)),
    ("Axis0_Press", "boolean", Press(Button::Axis0)),
    ("Axis0_Touch", "boolean", Touch(Button::Axis0)),
    ("Axis0_Value", "vector2", Axis(0)),
    ("Axis1_Press", "boolean", Press(Button::Axis1)),
    ("Axis1_Touch", "boolean", Touch(Button::Axis1)),
    ("Axis1_Value", "vector1", AxisX(1)),
    ("Axis2_Press", "boolean", Press(Button::Axis2)),
    ("Axis2_Touch", "boolean", Touch(Button::Axis2)),
    // The grip's value, or a second stick or trackpad.
    ("Axis2_Value1", "vector1", AxisX(2)),
    ("Axis2_Value2", "vector2", Axis(2)),
    ("Axis3_Press", "boolean", Press(Button::Axis3)),
    ("Axis3_Touch", "boolean", Touch(Button::Axis3)),
    ("Axis3_Value_e0", "vector1", AxisX(3)),
    ("Axis3_Value_e1", "vector1", AxisY(3)),
    ("Axis4_Press", "boolean", Press(Button::Axis4)),
    ("Axis4_Touch", "boolean", Touch(Button::Axis4)),
    ("Axis4_Value_e0", "vector1", AxisX(4)),
    ("Axis4_Value_e1", "vector1", AxisY(4)),
    ("Pose", "pose", Pose),
    ("Haptic", "vibration", Haptic),
];

/// The (lowercase) paths of a hand's action and of its mirrored action.
pub(in crate::input) fn action_paths(hand: Hand, name: &str, output: LegacyOutput) -> [String; 2] {
    let dir = if matches!(output, Haptic) {
        "out"
    } else {
        "in"
    };
    let hand = match hand {
        Hand::Left => "left",
        Hand::Right => "right",
    };
    let name = name.to_lowercase();
    [
        format!("{LEGACY_SET}/{dir}/{hand}_{name}"),
        format!("{MIRRORED_SET}/{dir}/{name}"),
    ]
}

/// The paths of the legacy action sets, to sync.
pub(in crate::input) const SETS: [&str; 2] = [LEGACY_SET, MIRRORED_SET];

fn manifest() -> actions::ActionManifest {
    let action = |name: String, ty: &str| serde_json::json!({ "name": name, "type": ty });
    // Not driven: the legacy controller state has no headset.
    let mut list = vec![action(format!("{LEGACY_SET}/in/head_proximity"), "boolean")];
    for hand in [Hand::Left, Hand::Right] {
        list.extend(
            HAND_ACTIONS
                .iter()
                .map(|(name, ty, output)| action(action_paths(hand, name, *output)[0].clone(), ty)),
        );
    }
    list.extend(
        HAND_ACTIONS.iter().map(|(name, ty, output)| {
            action(action_paths(Hand::Left, name, *output)[1].clone(), ty)
        }),
    );
    serde_json::from_value(serde_json::json!({
        "action_sets": SETS.map(|name| serde_json::json!({ "name": name })),
        "actions": list,
    }))
    .expect("legacy action manifest")
}

/// Reads legacy bindings, keeping the action set their mirror_actions option allows, like
/// SteamVR.
fn read_legacy_bindings(path: &Path, controller_type: &ControllerType) -> Option<Bindings> {
    let mut bindings = read_bindings(path, controller_type)?;
    let (set, other) = if bindings.options.mirror_actions {
        (MIRRORED_SET, LEGACY_SET)
    } else {
        (LEGACY_SET, MIRRORED_SET)
    };
    bindings.bindings.remove(other);
    if !bindings.bindings.contains_key(set) {
        warn!(
            "{} has no bindings for {set}, ignoring it for legacy input",
            path.display()
        );
        return None;
    }
    Some(bindings)
}

/// The interaction profiles of a controller type.
struct Profiles<'a>(&'a xr::Instance, Vec<xr::Path>);
impl RunWithProfile for Profiles<'_> {
    fn run<P: InteractionProfile>(&mut self) {
        self.1.extend(self.0.string_to_path(P::profile_path()));
    }
}

impl<C: openxr_data::Compositor> Input<C> {
    /// The legacy bindings chosen for the app's controllers (see [BindingSources]), and what was
    /// chosen.
    pub(in crate::input) fn chosen_legacy_bindings(
        &self,
    ) -> (Vec<(ControllerType, Bindings)>, BindingChoices) {
        let sources = BindingSources::new(self.app_key(), Category::Legacy);
        let chosen = ControllerType::known()
            .into_iter()
            .filter_map(|controller_type| {
                let path = sources.file_for(&controller_type)?;
                let bindings = read_legacy_bindings(&path, &controller_type)?;
                info!(
                    "Legacy input of {controller_type} controllers goes through {}",
                    path.display()
                );
                Some((controller_type, bindings))
            })
            .collect();
        (chosen, sources.into_choices())
    }

    /// Loads the legacy action manifest with the `chosen` bindings, and attaches it with the
    /// fixed legacy actions, which profiles without chosen bindings keep.
    pub(in crate::input) fn load_remapped_legacy(
        &self,
        session_data: &SessionData,
        fixed: LegacyActionData,
        chosen: Vec<(ControllerType, Bindings)>,
        choices: BindingChoices,
    ) -> Result<ManifestLoadedActions, vr::EVRInputError> {
        let mut remapped = HashSet::new();
        let load_bindings = |context: &mut BindingsLoadContext| {
            let mut variants = HashMap::new();
            for (controller_type, bindings) in &chosen {
                self.load_controller_bindings(controller_type, bindings, context, &mut variants);
                let Some(simulated) = bindings.options.simulated_controller_type.as_deref() else {
                    continue;
                };
                let Some(properties) = ControllerType::from_str(simulated)
                    .ok()
                    .and_then(|ty| ty.simulated_properties())
                else {
                    warn!("Can't simulate unknown controller type {simulated}");
                    continue;
                };
                let mut profiles = Profiles(&self.openxr.instance, Vec::new());
                controller_type.run_for_profile(&mut profiles);
                for profile in profiles.1 {
                    if context.suggested_profiles.contains(&profile) {
                        context.simulated_properties.insert(profile, properties);
                    }
                }
            }
            remapped = context.suggested_profiles.clone();
            self.suggest_fixed_legacy_bindings(&fixed, &session_data.input_data, &remapped);
            (variants, choices)
        };
        let mut loaded =
            self.create_manifest_actions(session_data, manifest(), load_bindings, &[&fixed.set])?;
        loaded.legacy = Some(Box::new(RemappedLegacy::new(self, remapped, fixed)));
        Ok(loaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every action of SteamVR's legacy manifest, in its case.
    #[test]
    fn action_names() {
        let manifest = manifest();
        assert_eq!(manifest.actions.len(), 1 + 3 * HAND_ACTIONS.len());
        assert_eq!(
            action_paths(Hand::Right, "Axis2_Value1", AxisX(2)),
            [
                "/actions/legacy/in/right_axis2_value1",
                "/actions/legacy_mirrored/in/axis2_value1"
            ]
        );
        assert_eq!(
            action_paths(Hand::Left, "Haptic", Haptic),
            [
                "/actions/legacy/out/left_haptic",
                "/actions/legacy_mirrored/out/haptic"
            ]
        );
    }
}
