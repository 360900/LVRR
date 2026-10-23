use super::{
    HmdProperties, InteractionProfile, MainAxisType, PoseTransformations, ProfileProperties,
    Property, SkeletalInputBindings, legal_paths, paths::*,
};
use crate::button_mask_from_ids;
use crate::input::BoundPoseType;
use crate::input::legacy::{self, LegacyBindings, button_mask_from_id};
use crate::input::profiles::DynInputPath;
use crate::openxr_data::Hand;
use glam::{EulerRot, Mat4, Quat, Vec3};
use openvr::EVRButtonId;
use std::iter::Iterator;

pub struct Knuckles;

impl InteractionProfile for Knuckles {
    // The finger curl sensors measure the fingers only partially.
    const SKELETAL_TRACKING_LEVEL: Option<openvr::EVRSkeletalTrackingLevel> =
        Some(openvr::EVRSkeletalTrackingLevel::Partial);
    const SENSES_FINGERS: bool = true;
    // From legacy_bindings_index_controller.json
    const LEGACY_TRIGGER_CLICK: Option<(f32, f32)> = Some((0.55, 0.5));
    type LegalPaths = legal_paths![
        Both::<
            (System, Click),
            (System, Touch),
            (A, Click),
            (A, Touch),
            (B, Click),
            (B, Touch),
            (Trigger, Click),
            (Trigger, Touch),
            (Trigger, Value),
            (Squeeze, Value),
            (Squeeze, Force),
            (Thumbstick, Click),
            (Thumbstick, Touch),
            (Thumbstick, ()),
            (Trackpad, Force),
            (Trackpad, Touch),
            (Trackpad, ()),
        >
    ];

    fn profile_path() -> &'static str {
        "/interaction_profiles/valve/index_controller"
    }
    fn has_required_extensions(_: &openxr::ExtensionSet) -> bool {
        true
    }
    fn properties() -> &'static ProfileProperties {
        static DEVICE_PROPERTIES: ProfileProperties = ProfileProperties {
            model: Property::PerHand {
                left: c"Knuckles Left",
                right: c"Knuckles Right",
            },
            openvr_controller_type: c"knuckles",
            render_model_name: Property::PerHand {
                left: c"{indexcontroller}valve_controller_knu_1_0_left",
                right: c"{indexcontroller}valve_controller_knu_1_0_right",
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
            tracking_system_name: c"lighthouse",
            manufacturer_name: c"Valve",
            input_profile_path: c"{indexcontroller}/input/index_controller_profile.json",
            // Like SteamVR, without A, which is reported as Grip.
            legacy_buttons_mask: button_mask_from_ids!(
                EVRButtonId::System,
                EVRButtonId::ApplicationMenu,
                EVRButtonId::Grip,
                EVRButtonId::Axis0,
                EVRButtonId::Axis1,
                EVRButtonId::Axis2
            ),
            hmd: HmdProperties {
                model: c"Index",
                controller_type: c"indexhmd",
                serial_number: c"LHR-FFFFFFF0",
            },
        };
        &DEVICE_PROPERTIES
    }
    fn translate_path(path: DynInputPath) -> Option<DynInputPath> {
        match path {
            p @ DynInputPath {
                subpath: DynSubpath::Trackpad,
                component: Some(DynComponent::Click),
                ..
            } => Some(p.with_component(DynComponent::Force)),
            p @ DynInputPath {
                subpath: DynSubpath::Squeeze,
                component: Some(DynComponent::Touch),
                ..
            } => Some(p.with_component(DynComponent::Value)),
            _ => None,
        }
    }

    fn legacy_bindings(c: &super::InputToXrPath<Self>) -> LegacyBindings {
        // Follows SteamVR's legacy_bindings_index_controller.json
        LegacyBindings {
            extra: legacy::Bindings {
                grip_pose: c.pose(),
            },
            app_menu: c.leftright::<B, Click, _, _>(),
            app_menu_touch: c.leftright::<B, Touch, _, _>(),
            a: vec![],
            a_touch: vec![],
            trigger: c.leftright::<Trigger, Value, _, _>(),
            trigger_click: c.leftright::<Trigger, Click, _, _>(),
            trigger_touch: c.leftright::<Trigger, Touch, _, _>(),
            // squeeze/value is the capacitive finger curl, which is already high when the
            // controller is merely held, so the grip is pressed through the force sensor.
            squeeze: c.leftright::<Squeeze, Force, _, _>(),
            squeeze_click: c.leftright::<Squeeze, Force, _, _>(),
            grip_click: c.leftright::<A, Click, _, _>(),
            axis2_touch: vec![],
            grip_touch: [
                c.leftright::<A, Touch, _, _>(),
                c.leftright::<Squeeze, Value, _, _>(),
            ]
            .concat(),
            // The stick and the trackpad both drive Axis0.
            main_xy: [
                c.leftright::<Thumbstick, (), _, _>(),
                c.leftright::<Trackpad, (), _, _>(),
            ]
            .concat(),
            main_xy_click: [
                c.leftright::<Thumbstick, Click, _, _>(),
                c.leftright::<Trackpad, Force, _, _>(),
            ]
            .concat(),
            main_xy_touch: [
                c.leftright::<Thumbstick, Touch, _, _>(),
                c.leftright::<Trackpad, Touch, _, _>(),
            ]
            .concat(),
            main_xy_deflect: c.leftright::<Thumbstick, (), _, _>(),
            haptic: c.haptics(),
        }
    }

    fn skeletal_input_bindings(c: &super::InputToXrPath<Self>) -> SkeletalInputBindings {
        SkeletalInputBindings {
            thumb_touch: c
                .leftright::<Thumbstick, Touch, _, _>()
                .into_iter()
                .chain(c.leftright::<Trackpad, Touch, _, _>())
                .chain(c.leftright::<A, Touch, _, _>())
                .chain(c.leftright::<B, Touch, _, _>())
                .collect(),
            index_touch: c.leftright::<Trigger, Touch, _, _>(),
            index_curl: c.leftright::<Trigger, Value, _, _>(),
            rest_curl: c.leftright::<Squeeze, Value, _, _>(),
        }
    }

    fn offset_grip_pose(hand: Hand) -> Mat4 {
        super::offset_grip_pose_from_pose_type::<Knuckles>(hand, BoundPoseType::Grip)
    }

    /// Derived from `SteamVR/drivers/indexcontroller/resources/rendermodels/valve_controller_knu_1_0*/*.json`
    fn pose_transformation(pose: BoundPoseType) -> Option<PoseTransformations> {
        match pose {
            BoundPoseType::Raw => None,
            // All 0 so skip transformation
            BoundPoseType::Gdc2015 => None,
            // Not present in rendermodels
            BoundPoseType::OpenxrAim => None,
            // Not present in rendermodels
            BoundPoseType::OpenxrGrip => None,
            BoundPoseType::Tip => Some(PoseTransformations {
                left_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        -40.0_f32.to_radians(),
                        -5.0_f32.to_radians(),
                        0_f32.to_radians(),
                    ),
                    Vec3::new(0.006, -0.015, 0.02),
                ),
                right_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        -40.0_f32.to_radians(),
                        5.0_f32.to_radians(),
                        0_f32.to_radians(),
                    ),
                    Vec3::new(-0.006, -0.015, 0.02),
                ),
            }),
            BoundPoseType::Base => Some(PoseTransformations {
                left_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        -155.4_f32.to_radians(),
                        -0.427_f32.to_radians(),
                        7.081_f32.to_radians(),
                    ),
                    Vec3::new(0.004758, -0.037977, 0.200466),
                ),
                right_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        -155.4_f32.to_radians(),
                        -0.427_f32.to_radians(),
                        -7.081_f32.to_radians(),
                    ),
                    Vec3::new(-0.004758, -0.037977, 0.200466),
                ),
            }),
            BoundPoseType::Handgrip => Some(PoseTransformations {
                left_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        15.392_f32.to_radians(),
                        -2.071_f32.to_radians(),
                        0.303_f32.to_radians(),
                    ),
                    Vec3::new(-0.003851, 0.003715, 0.075948),
                ),
                right_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        15.392_f32.to_radians(),
                        2.071_f32.to_radians(),
                        -0.303_f32.to_radians(),
                    ),
                    Vec3::new(0.003851, 0.003715, 0.075948),
                ),
            }),
            BoundPoseType::Grip => Some(PoseTransformations {
                left_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        15.392_f32.to_radians(),
                        -2.071_f32.to_radians(),
                        0.303_f32.to_radians(),
                    ),
                    Vec3::new(0.0, -0.015, 0.13),
                ),
                right_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        15.392_f32.to_radians(),
                        2.071_f32.to_radians(),
                        -0.303_f32.to_radians(),
                    ),
                    Vec3::new(0.0, -0.015, 0.13),
                ),
            }),
            BoundPoseType::OpenxrHandmodel => Some(PoseTransformations {
                left_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        -40_f32.to_radians(),
                        -5_f32.to_radians(),
                        0_f32.to_radians(),
                    ),
                    Vec3::new(-0.015, -0.015, 0.13),
                ),
                right_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        -40_f32.to_radians(),
                        5_f32.to_radians(),
                        0_f32.to_radians(),
                    ),
                    Vec3::new(0.015, -0.015, 0.13),
                ),
            }),
            BoundPoseType::OpenxrPinch => Some(PoseTransformations {
                left_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        -70_f32.to_radians(),
                        -15_f32.to_radians(),
                        35_f32.to_radians(),
                    ),
                    Vec3::new(0.05, 0.006, 0.0675),
                ),
                right_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        -70_f32.to_radians(),
                        15_f32.to_radians(),
                        -35_f32.to_radians(),
                    ),
                    Vec3::new(-0.05, 0.006, 0.0675),
                ),
            }),
            BoundPoseType::OpenxrPoke => Some(PoseTransformations {
                left_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        -60_f32.to_radians(),
                        0_f32.to_radians(),
                        0_f32.to_radians(),
                    ),
                    Vec3::new(-0.04, -0.0575, 0.0039),
                ),
                right_hand: Mat4::from_rotation_translation(
                    Quat::from_euler(
                        EulerRot::XYZ,
                        -60_f32.to_radians(),
                        0_f32.to_radians(),
                        0_f32.to_radians(),
                    ),
                    Vec3::new(0.04, -0.0575, 0.0039),
                ),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InteractionProfile, Knuckles};
    use crate::input::{ActionData, custom_bindings::BoolBindingType, tests::Fixture};
    use openxr as xr;

    #[test]
    fn verify_bindings() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");

        let path = Knuckles::profile_path();
        f.verify_bindings::<bool>(
            path,
            c"/actions/set1/in/boolact",
            [
                "/user/hand/left/input/a/click".into(),
                "/user/hand/right/input/a/click".into(),
                "/user/hand/left/input/b/click".into(),
                "/user/hand/right/input/b/click".into(),
                "/user/hand/left/input/trigger/touch".into(),
                "/user/hand/right/input/trigger/touch".into(),
                "/user/hand/left/input/thumbstick/click".into(),
                "/user/hand/right/input/thumbstick/click".into(),
                "/user/hand/left/input/thumbstick/touch".into(),
                "/user/hand/right/input/thumbstick/touch".into(),
                "/user/hand/right/input/trackpad/touch".into(),
                "/user/hand/left/input/trackpad/force".into(),
                "/user/hand/right/input/trackpad/force".into(),
            ],
        );

        f.verify_threshold_bindings(
            path,
            c"/actions/set1/in/boolact",
            [
                "/user/hand/left/input/trigger/value".into(),
                "/user/hand/right/input/trigger/value".into(),
                "/user/hand/left/input/squeeze/value".into(),
            ],
        );

        f.verify_bindings::<f32>(
            path,
            c"/user/hand/left/input/trackpad/force-/actions/set1",
            ["/user/hand/left/input/trackpad/force".into()],
        );

        let handle = f.get_action_handle(c"/actions/set1/in/boolact");
        let data = f.input.openxr.session_data.get();
        let actions = data.input_data.get_loaded_actions().unwrap();
        let ActionData::Bool(_) = actions.try_get_action(handle).unwrap() else {
            panic!("no");
        };

        let grab_force =
            f.custom_binding_action(handle, path, |ty| matches!(ty, BoolBindingType::Grab(_)));
        let p = f.input.openxr.instance.string_to_path(path).unwrap();
        let suggested = fakexr::get_suggested_bindings(grab_force, p);
        assert!(suggested.contains(&"/user/hand/right/input/squeeze/force".to_string()));

        f.verify_bindings::<f32>(
            path,
            c"/actions/set1/in/vec1act",
            [
                "/user/hand/left/input/trigger/value".into(),
                "/user/hand/right/input/trigger/value".into(),
                "/user/hand/left/input/squeeze/force".into(),
                "/user/hand/right/input/squeeze/value".into(),
            ],
        );

        f.verify_bindings::<xr::Vector2f>(
            path,
            c"/actions/set1/in/vec2act",
            [
                "/user/hand/left/input/trackpad".into(),
                "/user/hand/right/input/trackpad".into(),
                "/user/hand/left/input/thumbstick".into(),
                "/user/hand/right/input/thumbstick".into(),
            ],
        );

        // Scrolling reads the stick through an action of its own.
        let scrollact =
            f.get_action::<xr::Vector2f>(f.get_action_handle(c"/actions/set1/in/scrollact"));
        assert!(fakexr::check_no_suggested_bindings(scrollact, p));
        f.verify_bindings::<xr::Vector2f>(
            path,
            c"/user/hand/left/input/thumbstick-/actions/set1",
            ["/user/hand/left/input/thumbstick".into()],
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
