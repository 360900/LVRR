use super::{
    HmdProperties, InputToXrPath, InteractionProfile, MainAxisType, PoseTransformations,
    ProfileProperties, Property, SkeletalInputBindings, oculus_touch::OculusTouch,
};
use crate::button_mask_from_ids;
use crate::input::BoundPoseType;
use crate::input::legacy::{LegacyBindings, button_mask_from_id};
use crate::input::profiles::DynInputPath;
use crate::openxr_data::Hand;
use glam::Mat4;

/// The Quest Pro controllers (`XR_FB_touch_controller_pro`), which SteamVR reports as Touch
/// controllers with their own model.
pub struct TouchPro;

impl InteractionProfile for TouchPro {
    const LEFT_MENU_IS_SYSTEM: bool = true;
    // The Touch inputs, and some (proximity, curl, stylus) xrizer doesn't use.
    type LegalPaths = <OculusTouch as InteractionProfile>::LegalPaths;
    fn properties() -> &'static ProfileProperties {
        use openvr::EVRButtonId as btn;
        static DEVICE_PROPERTIES: ProfileProperties = ProfileProperties {
            // As SteamVR's vrlink driver names them.
            model: Property::PerHand {
                left: c"Oculus Quest Pro (Left Controller)",
                right: c"Oculus Quest Pro (Right Controller)",
            },
            openvr_controller_type: c"oculus_touch",
            render_model_name: Property::PerHand {
                left: c"oculus_quest_pro_controller_left",
                right: c"oculus_quest_pro_controller_right",
            },
            registered_device_type: Property::PerHand {
                left: c"oculus/WMHD315M3010GV_Controller_Left",
                right: c"oculus/WMHD315M3010GV_Controller_Right",
            },
            serial_number: Property::PerHand {
                left: c"WMHD315M3010GV_Controller_Left",
                right: c"WMHD315M3010GV_Controller_Right",
            },
            tracking_system_name: c"oculus",
            manufacturer_name: c"Oculus",
            input_profile_path: c"{oculus}/input/touch_profile.json",
            main_axis: MainAxisType::Thumbstick,
            legacy_buttons_mask: button_mask_from_ids!(
                btn::System,
                btn::ApplicationMenu,
                btn::Grip,
                btn::A,
                btn::Axis0,
                btn::Axis1,
                btn::Axis2
            ),
            hmd: HmdProperties {
                model: c"Oculus Quest Pro",
                controller_type: c"rift",
                serial_number: c"WMHD315M3010GV",
            },
        };
        &DEVICE_PROPERTIES
    }
    fn translate_path(path: DynInputPath) -> Option<DynInputPath> {
        OculusTouch::translate_path(path)
    }
    fn profile_path() -> &'static str {
        "/interaction_profiles/facebook/touch_controller_pro"
    }
    fn has_required_extensions(enabled_extensions: &openxr::ExtensionSet) -> bool {
        enabled_extensions.fb_touch_controller_pro
    }

    fn legacy_bindings(c: &InputToXrPath<Self>) -> LegacyBindings {
        OculusTouch::legacy_bindings(&InputToXrPath::new(c.instance))
    }

    fn skeletal_input_bindings(c: &InputToXrPath<Self>) -> SkeletalInputBindings {
        OculusTouch::skeletal_input_bindings(&InputToXrPath::new(c.instance))
    }

    fn offset_grip_pose(hand: Hand) -> Mat4 {
        super::offset_grip_pose_from_pose_type::<TouchPro>(hand, BoundPoseType::OpenxrGrip)
    }

    /// `SteamVR/resources/rendermodels/oculus_quest_pro_controller_*/*.json` has the same
    /// components as the Quest 2 models.
    fn pose_transformation(pose: BoundPoseType) -> Option<PoseTransformations> {
        OculusTouch::pose_transformation(pose)
    }
}

#[cfg(test)]
mod tests {
    use super::{InteractionProfile, TouchPro};
    use crate::input::tests::Fixture;

    #[test]
    fn touch_bindings() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");

        // The game's Touch bindings, as they are.
        f.verify_bindings::<bool>(
            TouchPro::profile_path(),
            c"/actions/set1/in/boolact",
            [
                "/user/hand/left/input/x/click".into(),
                "/user/hand/left/input/y/click".into(),
                "/user/hand/right/input/a/click".into(),
                "/user/hand/right/input/b/click".into(),
                "/user/hand/left/input/menu/click".into(),
                "/user/hand/left/input/trigger/value".into(),
                "/user/hand/left/input/thumbrest/touch".into(),
                "/user/hand/right/input/thumbstick/click".into(),
                "/user/hand/right/input/thumbstick/touch".into(),
            ],
        );
    }
}
