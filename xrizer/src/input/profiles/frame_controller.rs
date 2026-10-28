use super::{
    BoundPoseType, HmdProperties, InteractionProfile, Left, MainAxisType, PoseTransformations,
    ProfileProperties, Property, Right, SkeletalInputBindings, legal_paths, paths::*,
};
use crate::button_mask_from_ids;
use crate::input::legacy::{self, LegacyBindings, button_mask_from_id};
use crate::input::profiles::DynInputPath;
use crate::openxr_data::Hand;
use glam::{EulerRot, Mat4, Quat, Vec3};
use openvr::EVRButtonId;

pub struct FrameController;

impl InteractionProfile for FrameController {
    /// SteamVR's legacy bindings for it simulate the Touch controllers.
    fn legacy_simulated_properties() -> Option<&'static ProfileProperties> {
        Some(super::oculus_touch::OculusTouch::properties())
    }

    const SENSES_FINGERS: bool = true;
    // From legacy_bindings_frame_controller.json
    const LEGACY_TRIGGER_CLICK: Option<(f32, f32)> = Some((0.55, 0.5));
    const LEGACY_GRIP_CLICK: Option<(f32, f32)> = Some((0.1, 0.05));
    type LegalPaths = legal_paths![
        Both::<
            (System, Click),
            (System, Touch),
            (Bumper, Click),
            (Bumper, Touch),
            (Trigger, Click),
            (Trigger, Touch),
            (Trigger, Value),
            (Squeeze, Click),
            (Squeeze, Touch),
            (Squeeze, Value),
            (Thumbstick, Click),
            (Thumbstick, Touch),
            (Thumbstick, Vec2X),
            (Thumbstick, Vec2Y),
            (Thumbstick, ()),
        >,
        Left::<
            (DpadUp, Click),
            (DpadUp, Touch),
            (DpadLeft, Click),
            (DpadLeft, Touch),
            (DpadDown, Click),
            (DpadDown, Touch),
            (DpadRight, Click),
            (DpadRight, Touch),
            (View, Click),
            (View, Touch),
        >,
        Right::<
            (A, Click),
            (A, Touch),
            (B, Click),
            (B, Touch),
            (X, Click),
            (X, Touch),
            (Y, Click),
            (Y, Touch),
            (Menu, Click),
            (Menu, Touch),
        >
    ];

    fn profile_path() -> &'static str {
        "/interaction_profiles/valve/frame_controller_valve"
    }
    fn has_required_extensions(enabled_extensions: &openxr::ExtensionSet) -> bool {
        enabled_extensions
            .other
            .contains(&crate::openxr_data::XR_VALVE_FRAME_CONTROLLER_INTERACTION.to_vec())
    }
    fn properties() -> &'static ProfileProperties {
        static DEVICE_PROPERTIES: ProfileProperties = ProfileProperties {
            model: Property::PerHand {
                left: c"Frame Controller Left",
                right: c"Frame Controller Right",
            },
            openvr_controller_type: c"frame_controller",
            render_model_name: Property::PerHand {
                left: c"{frame_controller}frame_controller_left",
                right: c"{frame_controller}frame_controller_right",
            },
            main_axis: MainAxisType::Thumbstick,
            registered_device_type: Property::PerHand {
                left: c"valve/index_controllerLHR-FFFFFFF1",
                right: c"valve/index_controllerLHR-FFFFFFF2",
            },
            serial_number: Property::PerHand {
                left: c"LHR-FFFFFFF1",
                right: c"LHR-FFFFFFF2",
            },
            tracking_system_name: c"cv",
            manufacturer_name: c"Valve",
            legacy_buttons_mask: button_mask_from_ids!(
                EVRButtonId::System,
                EVRButtonId::ApplicationMenu,
                EVRButtonId::Grip,
                EVRButtonId::A,
                EVRButtonId::Axis0,
                EVRButtonId::Axis1,
                EVRButtonId::Axis2
            ),
            hmd: HmdProperties {
                model: c"Steam Frame",
                controller_type: c"frame_controller",
                serial_number: c"LHR-FFFFFFF0",
                render_model_name: c"generic_hmd",
                registered_device_type: c"",
                input_profile_path: c"{frame_hmd}/input/input_profile.json",
            },
            input_profile_path: c"{frame_controller}/input/frame_controller_profile.json",
        };
        &DEVICE_PROPERTIES
    }
    fn translate_path(_path: DynInputPath) -> Option<DynInputPath> {
        None
    }

    fn legacy_bindings(c: &super::InputToXrPath<Self>) -> LegacyBindings {
        // Follows SteamVR's legacy_bindings_frame_controller.json
        LegacyBindings {
            extra: legacy::Bindings {
                grip_pose: c.pose(),
            },
            app_menu: [
                c.into::<Left<DpadLeft, Click>, _>(),
                c.into::<Left<DpadRight, Click>, _>(),
                c.into::<Left<DpadUp, Click>, _>(),
                c.into::<Right<B, Click>, _>(),
                c.into::<Right<X, Click>, _>(),
                c.into::<Right<Y, Click>, _>(),
            ]
            .concat(),
            app_menu_touch: [
                c.into::<Left<DpadLeft, Touch>, _>(),
                c.into::<Left<DpadRight, Touch>, _>(),
                c.into::<Left<DpadUp, Touch>, _>(),
                c.into::<Right<B, Touch>, _>(),
                c.into::<Right<X, Touch>, _>(),
                c.into::<Right<Y, Touch>, _>(),
            ]
            .concat(),
            a: [
                c.into::<Left<DpadDown, Click>, _>(),
                c.into::<Right<A, Click>, _>(),
            ]
            .concat(),
            a_touch: [
                c.into::<Left<DpadDown, Touch>, _>(),
                c.into::<Right<A, Touch>, _>(),
            ]
            .concat(),
            // The bumpers act as a fully pulled trigger.
            trigger: [
                c.leftright::<Trigger, Value, _, _>(),
                c.leftright::<Bumper, Click, _, _>(),
            ]
            .concat(),
            trigger_click: [
                c.leftright::<Trigger, Click, _, _>(),
                c.leftright::<Bumper, Click, _, _>(),
            ]
            .concat(),
            trigger_touch: [
                c.leftright::<Trigger, Touch, _, _>(),
                c.leftright::<Bumper, Touch, _, _>(),
            ]
            .concat(),
            squeeze: c.leftright::<Squeeze, Value, _, _>(),
            squeeze_click: c.leftright::<Squeeze, Value, _, _>(),
            grip_click: vec![],
            axis2_touch: vec![],
            grip_touch: c.leftright::<Squeeze, Touch, _, _>(),
            main_xy: c.leftright::<Thumbstick, (), _, _>(),
            main_xy_click: c.leftright::<Thumbstick, Click, _, _>(),
            main_xy_touch: c.leftright::<Thumbstick, Touch, _, _>(),
            main_xy_deflect: vec![],
            haptic: c.haptics(),
        }
    }

    fn skeletal_input_bindings(c: &super::InputToXrPath<Self>) -> SkeletalInputBindings {
        SkeletalInputBindings {
            thumb_touch: [
                c.leftright::<Thumbstick, Touch, _, _>(),
                c.into::<Left<DpadUp, Touch>, _>(),
                c.into::<Left<DpadLeft, Touch>, _>(),
                c.into::<Left<DpadDown, Touch>, _>(),
                c.into::<Left<DpadRight, Touch>, _>(),
                c.into::<Right<A, Touch>, _>(),
                c.into::<Right<B, Touch>, _>(),
                c.into::<Right<X, Touch>, _>(),
                c.into::<Right<Y, Touch>, _>(),
            ]
            .concat(),
            index_touch: c.leftright::<Trigger, Touch, _, _>(),
            index_curl: c.leftright::<Trigger, Value, _, _>(),
            rest_curl: c.leftright::<Squeeze, Value, _, _>(),
        }
    }

    fn offset_grip_pose(hand: Hand) -> Mat4 {
        match hand {
            Hand::Left => Mat4::from_rotation_translation(
                Quat::from_euler(
                    EulerRot::XYZ,
                    15.392_f32.to_radians(),
                    -2.071_f32.to_radians(),
                    0.303_f32.to_radians(),
                ),
                Vec3::new(0.0, -0.015, 0.13),
            )
            .inverse(),
            Hand::Right => Mat4::from_rotation_translation(
                Quat::from_euler(
                    EulerRot::XYZ,
                    15.392_f32.to_radians(),
                    2.071_f32.to_radians(),
                    -0.303_f32.to_radians(),
                ),
                Vec3::new(0.0, -0.015, 0.13),
            )
            .inverse(),
        }
    }

    // TODO: Source the real pose transformations (tip, base, ...) from the
    // Steam Frame controller driver JSON; until then all pose types report the
    // raw pose.
    fn pose_transformation(_pose: BoundPoseType) -> Option<PoseTransformations> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameController, InteractionProfile};
    use crate::input::tests::Fixture;
    use openxr as xr;

    #[test]
    fn verify_bindings() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");

        let path = FrameController::profile_path();
        f.verify_bindings::<bool>(
            path,
            c"/actions/set1/in/boolact",
            [
                "/user/hand/right/input/a/click".into(),
                "/user/hand/right/input/b/click".into(),
                "/user/hand/right/input/x/click".into(),
                "/user/hand/right/input/y/click".into(),
                "/user/hand/right/input/menu/click".into(),
                "/user/hand/left/input/dpad_up/click".into(),
                "/user/hand/left/input/dpad_down/click".into(),
                "/user/hand/left/input/view/click".into(),
                "/user/hand/left/input/bumper/click".into(),
                "/user/hand/right/input/bumper/touch".into(),
                "/user/hand/left/input/thumbstick/click".into(),
            ],
        );

        f.verify_bindings::<f32>(
            path,
            c"/actions/set1/in/vec1act",
            [
                "/user/hand/left/input/trigger/value".into(),
                "/user/hand/right/input/squeeze/value".into(),
            ],
        );

        f.verify_bindings::<xr::Vector2f>(
            path,
            c"/actions/set1/in/vec2act",
            [
                "/user/hand/left/input/thumbstick".into(),
                "/user/hand/right/input/thumbstick".into(),
            ],
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
