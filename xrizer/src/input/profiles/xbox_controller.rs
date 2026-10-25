//! The Xbox controller (core OpenXR, `/user/gamepad`), which SteamVR's gamepad driver serves as
//! `gamepad` (`drivers/gamepad/resources/input/gamepad_profile.json`). That driver is off by
//! default (`driver_gamepad` in `resources/settings/default.vrsettings`) and its device has no
//! pose, so xrizer adds no tracked device for the gamepad: it only drives actions.

use super::{
    BoundPoseType, Gamepad, HmdProperties, InputToXrPath, InteractionProfile, MainAxisType,
    PoseTransformations, ProfileProperties, Property, SkeletalInputBindings, legal_paths, paths::*,
};
use crate::input::legacy::{self, LegacyBindings};
use crate::openxr_data::Hand;
use glam::Mat4;

pub struct XboxController;

/// The input xrizer reads to tell whether a gamepad is connected. Any of them would do: WiVRn
/// makes them all active at once.
pub const PRESENCE_INPUT: &str = "/user/gamepad/input/a/click";

impl InteractionProfile for XboxController {
    const GAMEPAD: bool = true;
    type LegalPaths = legal_paths![
        Gamepad::<
            (Menu, Click),
            (View, Click),
            (A, Click),
            (B, Click),
            (X, Click),
            (Y, Click),
            (DpadDown, Click),
            (DpadRight, Click),
            (DpadUp, Click),
            (DpadLeft, Click),
            (ShoulderLeft, Click),
            (ShoulderRight, Click),
            (ThumbstickLeft, Click),
            (ThumbstickRight, Click),
            (TriggerLeft, Value),
            (TriggerRight, Value),
            (ThumbstickLeft, ()),
            (ThumbstickRight, ()),
        >
    ];

    /// Only the controller type and input profile, from SteamVR's driver, are known. Without a
    /// tracked device, only the controller type is ever reported (by GetActionBindingInfo).
    fn properties() -> &'static ProfileProperties {
        static DEVICE_PROPERTIES: ProfileProperties = ProfileProperties {
            model: Property::BothHands(c""),
            openvr_controller_type: c"gamepad",
            render_model_name: Property::BothHands(c""),
            main_axis: MainAxisType::Thumbstick,
            registered_device_type: Property::BothHands(c""),
            serial_number: Property::BothHands(c""),
            tracking_system_name: c"",
            manufacturer_name: c"",
            input_profile_path: c"{gamepad}/input/gamepad_profile.json",
            legacy_buttons_mask: 0,
            hmd: HmdProperties {
                model: c"",
                controller_type: c"",
                serial_number: c"",
                render_model_name: c"",
                registered_device_type: c"",
                input_profile_path: c"",
            },
        };
        &DEVICE_PROPERTIES
    }
    fn profile_path() -> &'static str {
        "/interaction_profiles/microsoft/xbox_controller"
    }
    fn has_required_extensions(_: &openxr::ExtensionSet) -> bool {
        true
    }

    /// SteamVR's gamepad has no legacy bindings (`"legacy_profile": "none"`). These only feed
    /// xrizer's own actions: the haptic action plays binding haptics on both rumble motors, like
    /// SteamVR's driver rumbles the whole gamepad, and the info action needs an input.
    fn legacy_bindings(c: &InputToXrPath<Self>) -> LegacyBindings {
        LegacyBindings {
            extra: legacy::Bindings { grip_pose: vec![] },
            trigger: vec![],
            trigger_click: c.into::<Gamepad<TriggerRight, Value>, _>(),
            app_menu: vec![],
            a: vec![],
            squeeze_click: vec![],
            squeeze: vec![],
            main_xy: vec![],
            main_xy_click: vec![],
            main_xy_touch: vec![],
            trigger_touch: vec![],
            app_menu_touch: vec![],
            a_touch: vec![],
            axis2_touch: vec![],
            grip_touch: vec![],
            grip_click: vec![],
            main_xy_deflect: vec![],
            haptic: c.gamepad_haptics(),
        }
    }

    fn skeletal_input_bindings(_: &InputToXrPath<Self>) -> SkeletalInputBindings {
        SkeletalInputBindings {
            thumb_touch: vec![],
            index_touch: vec![],
            index_curl: vec![],
            rest_curl: vec![],
        }
    }

    fn offset_grip_pose(_: Hand) -> Mat4 {
        Mat4::IDENTITY
    }

    fn pose_transformation(_: BoundPoseType) -> Option<PoseTransformations> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{InteractionProfile, XboxController};
    use crate::input::profiles::oculus_touch::OculusTouch;
    use crate::input::tests::Fixture;
    use openxr as xr;

    #[test]
    fn verify_bindings() {
        let f = Fixture::new();
        f.load_actions(c"actions_gamepad.json");
        let path = XboxController::profile_path();

        // A hand's input in the gamepad's bindings is left out.
        f.verify_bindings::<bool>(
            path,
            c"/actions/main/in/jump",
            ["/user/gamepad/input/a/click".into()],
        );
        f.verify_threshold_bindings(
            path,
            c"/actions/main/in/fire",
            ["/user/gamepad/input/trigger_right/value".into()],
        );
        f.verify_bindings::<f32>(
            path,
            c"/actions/main/in/throttle",
            ["/user/gamepad/input/trigger_left/value".into()],
        );
        f.verify_bindings::<xr::Vector2f>(
            path,
            c"/actions/main/in/move",
            ["/user/gamepad/input/thumbstick_left".into()],
        );
        f.verify_custom_bindings(
            path,
            c"/actions/main/in/turn",
            ["/user/gamepad/input/thumbstick_right".into()],
        );
        // Both rumble motors, and not the hand's haptic.
        f.verify_bindings::<xr::Haptic>(
            path,
            c"/actions/main/in/vib",
            [
                "/user/gamepad/output/haptic_left".into(),
                "/user/gamepad/output/haptic_right".into(),
            ],
        );

        // The hands keep their own.
        let touch = OculusTouch::profile_path();
        f.verify_bindings::<bool>(
            touch,
            c"/actions/main/in/jump",
            ["/user/hand/right/input/a/click".into()],
        );
        f.verify_bindings::<xr::Haptic>(
            touch,
            c"/actions/main/in/vib",
            [
                "/user/hand/left/output/haptic".into(),
                "/user/hand/right/output/haptic".into(),
            ],
        );
    }

    #[test]
    fn no_bindings_without_gamepad_bindings() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");
        let profile = f
            .input
            .openxr
            .instance
            .string_to_path(XboxController::profile_path())
            .unwrap();
        let boolact = f.get_action_handle(c"/actions/set1/in/boolact");
        assert!(fakexr::check_no_suggested_bindings(
            f.get_action::<bool>(boolact),
            profile
        ));
    }
}
