use super::super::{
    HmdProperties, InteractionProfile, MainAxisType, PoseTransformations, ProfileProperties,
    Property, SkeletalInputBindings, legal_paths, paths::*,
};
use crate::button_mask_from_ids;
use crate::input::BoundPoseType;
use crate::input::legacy::{self, LegacyBindings, button_mask_from_id};
use crate::input::profiles::DynInputPath;
use crate::input::profiles::wmr;
use crate::openxr_data::Hand;
use glam::{Mat4, Quat, Vec3};
use openvr::EVRButtonId;
use openxr as xr;

pub struct ReverbG2Controller;

impl InteractionProfile for ReverbG2Controller {
    type LegalPaths = legal_paths![
        Both::<
            (Squeeze, Value),
            (Trigger, Value),
            (Thumbstick, Vec2X),
            (Thumbstick, Vec2Y),
            (Thumbstick, Click),
            (Thumbstick, ()),
        >,
        Left::<(X, Click), (Y, Click), (Menu, Click)>,
        Right::<(A, Click), (B, Click), (Menu, Click)>
    ];

    fn profile_path() -> &'static str {
        "/interaction_profiles/hp/mixed_reality_controller"
    }

    fn has_required_extensions(enabled_extensions: &xr::ExtensionSet) -> bool {
        enabled_extensions.ext_hp_mixed_reality_controller
    }

    fn properties() -> &'static ProfileProperties {
        static DEVICE_PROPERTIES: ProfileProperties = ProfileProperties {
            model: Property::PerHand {
                left: c"WindowsMR: 0x045E/0x066A/0/1",
                right: c"WindowsMR: 0x045E/0x066A/0/2",
            },
            openvr_controller_type: c"hpmotioncontroller",
            render_model_name: Property::PerHand {
                left: c"C:\\Users\\steamuser\\AppData\\Local\\Microsoft/Windows/OpenVR\\controller_1642_1118_1\\controller.obj",
                right: c"C:\\Users\\steamuser\\AppData\\Local\\Microsoft/Windows/OpenVR\\controller_1642_1118_2\\controller.obj",
            },
            main_axis: MainAxisType::Thumbstick,
            registered_device_type: Property::PerHand {
                left: c"WindowsMR/MRSOURCE0",
                right: c"WindowsMR/MRSOURCE1",
            },
            serial_number: wmr::SERIAL_NUMBER,
            tracking_system_name: wmr::TRACKING_SYSTEM_NAME,
            manufacturer_name: c"WindowsMR: 0x045E",
            input_profile_path: c"",
            legacy_buttons_mask: button_mask_from_ids!(
                EVRButtonId::System,
                EVRButtonId::ApplicationMenu,
                EVRButtonId::Grip,
                EVRButtonId::Axis0,
                EVRButtonId::Axis1,
                EVRButtonId::Axis2,
                EVRButtonId::A
            ),
            hmd: HmdProperties {
                model: c"HP Reverb G2",
                controller_type: c"hpmotioncontroller",
                serial_number: c"MRSOURCE0",
            },
        };
        &DEVICE_PROPERTIES
    }

    fn translate_path(path: DynInputPath) -> Option<DynInputPath> {
        match path {
            p @ DynInputPath {
                subpath: DynSubpath::Trigger,
                component: Some(DynComponent::Click),
                ..
            } => Some(p.with_component(DynComponent::Value)),
            p @ DynInputPath {
                subpath: DynSubpath::Squeeze,
                component: Some(DynComponent::Click),
                ..
            } => Some(p.with_component(DynComponent::Value)),
            _ => None,
        }
    }

    fn legacy_bindings(c: &super::super::InputToXrPath<Self>) -> LegacyBindings {
        LegacyBindings {
            extra: legacy::Bindings {
                grip_pose: c.pose(),
            },
            app_menu: c.leftright::<Menu, Click, _, _>(),
            a: c.into::<super::super::Left<X, Click>, _>()
                .into_iter()
                .chain(c.into::<super::super::Right<A, Click>, _>())
                .collect(),
            trigger: c.leftright::<Trigger, Value, _, _>(),
            trigger_click: c.leftright::<Trigger, Value, _, _>(),
            squeeze: c.leftright::<Squeeze, Value, _, _>(),
            squeeze_click: c.leftright::<Squeeze, Value, _, _>(),
            main_xy: c.leftright::<Thumbstick, (), _, _>(),
            main_xy_click: c.leftright::<Thumbstick, Click, _, _>(),
            main_xy_touch: vec![],
            app_menu_touch: vec![],
            a_touch: vec![],
            trigger_touch: vec![],
            grip_touch: vec![],
            haptic: c.haptics(),
        }
    }

    fn skeletal_input_bindings(c: &super::super::InputToXrPath<Self>) -> SkeletalInputBindings {
        SkeletalInputBindings {
            thumb_touch: c
                .leftright::<Thumbstick, Click, _, _>()
                .into_iter()
                .chain(c.into::<super::super::Left<X, Click>, _>())
                .chain(c.into::<super::super::Left<Y, Click>, _>())
                .chain(c.into::<super::super::Right<A, Click>, _>())
                .chain(c.into::<super::super::Right<B, Click>, _>())
                .collect(),
            index_touch: c.leftright::<Trigger, Value, _, _>(),
            index_curl: c.leftright::<Trigger, Value, _, _>(),
            rest_curl: c.leftright::<Squeeze, Value, _, _>(),
        }
    }

    fn offset_grip_pose(hand: Hand) -> Mat4 {
        Mat4::from_rotation_translation(
            Quat::from_xyzw(0.300705, 0.0, 0.0, 0.953717),
            Vec3::new(
                0.000683
                    * match hand {
                        Hand::Left => 1.0,
                        Hand::Right => -1.0,
                    },
                -0.015332,
                0.068270,
            ),
        )
    }

    fn pose_transformation(_pose: BoundPoseType) -> Option<PoseTransformations> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::tests::Fixture;

    #[test]
    fn verify_bindings() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");
        let path = ReverbG2Controller::profile_path();
        f.verify_bindings::<bool>(
            path,
            c"/actions/set1/in/boolact",
            [
                "/user/hand/left/input/thumbstick/click".into(),
                "/user/hand/right/input/thumbstick/click".into(),
                "/user/hand/left/input/menu/click".into(),
                "/user/hand/right/input/menu/click".into(),
                "/user/hand/left/input/x/click".into(),
                "/user/hand/left/input/y/click".into(),
                "/user/hand/right/input/a/click".into(),
                "/user/hand/right/input/b/click".into(),
            ],
        );
        f.verify_bindings::<f32>(
            path,
            c"/actions/set1/boolact_asfloat",
            [
                "/user/hand/left/input/squeeze/value".into(),
                "/user/hand/right/input/squeeze/value".into(),
                "/user/hand/left/input/trigger/value".into(),
                "/user/hand/right/input/trigger/value".into(),
            ],
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
