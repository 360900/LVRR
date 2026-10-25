//! Pico controllers (`XR_BD_controller_interaction`), which SteamVR serves through its vrlink
//! driver as `pico_controller` (`drivers/vrlink/resources/input/pico_controller_profile.json`).

use super::{
    HmdProperties, InputToXrPath, InteractionProfile, Left, MainAxisType, PoseTransformations,
    ProfileProperties, Property, Right, SkeletalInputBindings, legal_paths,
    oculus_touch::OculusTouch, paths::*,
};
use crate::button_mask_from_ids;
use crate::input::BoundPoseType;
use crate::input::legacy::{self, LegacyBindings, button_mask_from_id};
use crate::openxr_data::Hand;
use glam::Mat4;
use openvr::EVRButtonId as btn;

const PICO4_PROPERTIES: ProfileProperties = ProfileProperties {
    // As the vrlink driver names them.
    model: Property::PerHand {
        left: c"PICO 4 (Left Controller)",
        right: c"PICO 4 (Right Controller)",
    },
    openvr_controller_type: c"pico_controller",
    // The vrlink models of the Pico 4 controllers. The Neo 3 has none of its own there.
    render_model_name: Property::PerHand {
        left: c"pico_4_controller_left",
        right: c"pico_4_controller_right",
    },
    // These four from the `simulation_settings` of SteamVR's
    // `resources/input/default_pico_controller_profile.json`, its only record of them.
    registered_device_type: Property::PerHand {
        left: c"pico/PICO 4 Controller left",
        right: c"pico/PICO 4 Controller right",
    },
    serial_number: Property::PerHand {
        left: c"PICO_CONTROLLER_LEFT",
        right: c"PICO_CONTROLLER_RIGHT",
    },
    tracking_system_name: c"oculus",
    manufacturer_name: c"Oculus",
    input_profile_path: c"{vrlink}/input/pico_controller_profile.json",
    main_axis: MainAxisType::Thumbstick,
    // The `legacy_buttons` of the same file, which are the Touch's.
    legacy_buttons_mask: button_mask_from_ids!(
        btn::System,
        btn::ApplicationMenu,
        btn::Grip,
        btn::A,
        btn::Axis0,
        btn::Axis1,
        btn::Axis2
    ),
    // The controller type is the file's `hmd_profile`. TODO: The serial number is a guess, not
    // pulled from a real system report
    hmd: HmdProperties {
        model: c"PICO 4",
        controller_type: c"rift",
        serial_number: c"PICO_HMD",
    },
};

/// The Pico 4. The Pico 4 Ultra reports this profile on WiVRn too, since Monado has none for
/// its own (`pico4s_controller`, which isn't in the OpenXR registry either), so it gets these
/// poses. SteamVR's `pico_4_ultra_controller` models have no `openxr_grip` to take its own from.
pub struct Pico4;

impl InteractionProfile for Pico4 {
    const SIMULATE_CONVERTED: bool = true;
    type LegalPaths = legal_paths![
        Both::<
            (Squeeze, Value),
            (Squeeze, Click),
            (Trigger, Value),
            (Trigger, Click),
            (Trigger, Touch),
            (Thumbstick, ()),
            (Thumbstick, Click),
            (Thumbstick, Touch),
            (System, Click),
        >,
        Left::<(X, Click), (X, Touch), (Y, Click), (Y, Touch), (Menu, Click)>,
        Right::<(A, Click), (A, Touch), (B, Click), (B, Touch)>
    ];

    fn properties() -> &'static ProfileProperties {
        static DEVICE_PROPERTIES: ProfileProperties = PICO4_PROPERTIES;
        &DEVICE_PROPERTIES
    }
    fn profile_path() -> &'static str {
        "/interaction_profiles/bytedance/pico4_controller"
    }
    fn has_required_extensions(enabled_extensions: &openxr::ExtensionSet) -> bool {
        enabled_extensions.bd_controller_interaction
    }

    /// Like SteamVR's `legacy_bindings_pico_controller.json`, which are the Touch's.
    fn legacy_bindings(c: &InputToXrPath<Self>) -> LegacyBindings {
        LegacyBindings {
            extra: legacy::Bindings {
                grip_pose: c.pose(),
            },
            trigger: c.leftright::<Trigger, Value, _, _>(),
            trigger_click: c.leftright::<Trigger, Value, _, _>(),
            app_menu: [
                c.into::<Left<Y, Click>, _>(),
                c.into::<Right<B, Click>, _>(),
            ]
            .concat(),
            a: [
                c.into::<Left<X, Click>, _>(),
                c.into::<Right<A, Click>, _>(),
            ]
            .concat(),
            squeeze_click: c.leftright::<Squeeze, Value, _, _>(),
            squeeze: c.leftright::<Squeeze, Value, _, _>(),
            main_xy: c.leftright::<Thumbstick, (), _, _>(),
            main_xy_click: c.leftright::<Thumbstick, Click, _, _>(),
            main_xy_touch: c.leftright::<Thumbstick, Touch, _, _>(),
            trigger_touch: c.leftright::<Trigger, Touch, _, _>(),
            app_menu_touch: [
                c.into::<Left<Y, Touch>, _>(),
                c.into::<Right<B, Touch>, _>(),
            ]
            .concat(),
            a_touch: [
                c.into::<Left<X, Touch>, _>(),
                c.into::<Right<A, Touch>, _>(),
            ]
            .concat(),
            axis2_touch: vec![],
            grip_touch: vec![],
            grip_click: vec![],
            main_xy_deflect: vec![],
            haptic: c.haptics(),
        }
    }

    fn skeletal_input_bindings(c: &InputToXrPath<Self>) -> SkeletalInputBindings {
        SkeletalInputBindings {
            thumb_touch: [
                c.leftright::<Thumbstick, Touch, _, _>(),
                c.into::<Left<X, Touch>, _>(),
                c.into::<Left<Y, Touch>, _>(),
                c.into::<Right<A, Touch>, _>(),
                c.into::<Right<B, Touch>, _>(),
            ]
            .concat(),
            index_touch: c.leftright::<Trigger, Touch, _, _>(),
            index_curl: c.leftright::<Trigger, Value, _, _>(),
            rest_curl: c.leftright::<Squeeze, Value, _, _>(),
        }
    }

    fn offset_grip_pose(hand: Hand) -> Mat4 {
        super::offset_grip_pose_from_pose_type::<Pico4>(hand, BoundPoseType::OpenxrGrip)
    }

    /// `SteamVR/drivers/vrlink/resources/rendermodels/pico_4_controller_*/*.json` has the
    /// Quest 2 components, so these are the Touch's, as when WiVRn reported Pico controllers as
    /// Touch. It has no pinch and poke, which stay the Touch's too.
    fn pose_transformation(pose: BoundPoseType) -> Option<PoseTransformations> {
        OculusTouch::pose_transformation(pose)
    }
}

/// The Pico Neo 3, whose controllers have a menu button on both hands.
pub struct PicoNeo3;

impl InteractionProfile for PicoNeo3 {
    const SIMULATE_CONVERTED: bool = true;
    type LegalPaths = legal_paths![
        Both::<
            (Squeeze, Value),
            (Squeeze, Click),
            (Trigger, Value),
            (Trigger, Click),
            (Trigger, Touch),
            (Thumbstick, ()),
            (Thumbstick, Click),
            (Thumbstick, Touch),
            (System, Click),
            (Menu, Click),
        >,
        Left::<(X, Click), (X, Touch), (Y, Click), (Y, Touch)>,
        Right::<(A, Click), (A, Touch), (B, Click), (B, Touch)>
    ];

    fn properties() -> &'static ProfileProperties {
        static DEVICE_PROPERTIES: ProfileProperties = ProfileProperties {
            model: Property::PerHand {
                left: c"PICO Neo 3 (Left Controller)",
                right: c"PICO Neo 3 (Right Controller)",
            },
            hmd: HmdProperties {
                model: c"PICO Neo 3",
                ..PICO4_PROPERTIES.hmd
            },
            ..PICO4_PROPERTIES
        };
        &DEVICE_PROPERTIES
    }
    fn profile_path() -> &'static str {
        "/interaction_profiles/bytedance/pico_neo3_controller"
    }
    fn has_required_extensions(enabled_extensions: &openxr::ExtensionSet) -> bool {
        enabled_extensions.bd_controller_interaction
    }

    // The Pico 4 inputs are all here too.
    fn legacy_bindings(c: &InputToXrPath<Self>) -> LegacyBindings {
        Pico4::legacy_bindings(&InputToXrPath::new(c.instance))
    }

    fn skeletal_input_bindings(c: &InputToXrPath<Self>) -> SkeletalInputBindings {
        Pico4::skeletal_input_bindings(&InputToXrPath::new(c.instance))
    }

    fn offset_grip_pose(hand: Hand) -> Mat4 {
        Pico4::offset_grip_pose(hand)
    }

    fn pose_transformation(pose: BoundPoseType) -> Option<PoseTransformations> {
        Pico4::pose_transformation(pose)
    }
}

#[cfg(test)]
mod tests {
    use super::{InteractionProfile, Pico4, PicoNeo3};
    use crate::input::tests::Fixture;
    use openxr as xr;

    #[test]
    fn verify_bindings() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");

        for (path, right_menu) in [
            (Pico4::profile_path(), None),
            (
                PicoNeo3::profile_path(),
                Some("/user/hand/right/input/menu/click"),
            ),
        ] {
            f.verify_bindings::<bool>(
                path,
                c"/actions/set1/in/boolact",
                [
                    "/user/hand/left/input/x/click",
                    "/user/hand/left/input/y/click",
                    "/user/hand/right/input/a/click",
                    "/user/hand/right/input/b/click",
                    "/user/hand/left/input/menu/click",
                    "/user/hand/right/input/thumbstick/click",
                    "/user/hand/right/input/thumbstick/touch",
                ]
                .into_iter()
                .chain(right_menu)
                .map(String::from)
                .collect::<std::collections::HashSet<_>>(),
            );

            f.verify_threshold_bindings(
                path,
                c"/actions/set1/in/boolact",
                [
                    "/user/hand/left/input/squeeze/value".into(),
                    "/user/hand/right/input/trigger/value".into(),
                ],
            );

            // The grip has no touch sensor.
            f.verify_bindings::<bool>(
                path,
                c"/actions/set1/in/boolact2",
                ["/user/hand/left/input/trigger/touch".into()],
            );

            f.verify_bindings::<f32>(
                path,
                c"/actions/set1/in/vec1act",
                [
                    "/user/hand/left/input/trigger/value".into(),
                    "/user/hand/right/input/trigger/value".into(),
                ],
            );

            f.verify_bindings::<xr::Vector2f>(
                path,
                c"/actions/set1/in/vec2act",
                ["/user/hand/right/input/thumbstick".into()],
            );

            f.verify_bindings::<xr::Haptic>(
                path,
                c"/actions/set1/in/vib",
                [
                    "/user/hand/left/output/haptic".into(),
                    "/user/hand/right/output/haptic".into(),
                ],
            );
        }
    }
}
