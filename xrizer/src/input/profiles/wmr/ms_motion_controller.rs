use super::super::{
    HmdProperties, InteractionProfile, MainAxisType, PoseTransformations, ProfileProperties,
    Property, SkeletalInputBindings, legal_paths, paths::*,
};
use crate::input::BoundPoseType;
use crate::input::legacy::{self, LegacyBindings};
use crate::input::profiles::DynInputPath;
use crate::input::profiles::wmr;
use crate::openxr_data::Hand;
use glam::{Mat4, Vec3};
use openxr as xr;

pub struct HolographicController;

impl InteractionProfile for HolographicController {
    type LegalPaths = legal_paths![
        Both::<
            (Menu, Click),
            (Squeeze, Click),
            (Trigger, Value),
            (Thumbstick, Vec2X),
            (Thumbstick, Vec2Y),
            (Thumbstick, Click),
            (Thumbstick, ()),
            (Trackpad, Vec2X),
            (Trackpad, Vec2Y),
            (Trackpad, Click),
            (Trackpad, Touch),
            (Trackpad, ()),
        >
    ];

    fn profile_path() -> &'static str {
        "/interaction_profiles/microsoft/motion_controller"
    }

    fn has_required_extensions(_enabled_extensions: &xr::ExtensionSet) -> bool {
        true
    }

    fn properties() -> &'static ProfileProperties {
        static DEVICE_PROPERTIES: ProfileProperties = ProfileProperties {
            model: Property::PerHand {
                left: c"WindowsMR: 0x045E/0x065B/0/1",
                right: c"WindowsMR: 0x045E/0x065B/0/2",
            },
            openvr_controller_type: wmr::OG_OPENVR_CONTROLLER_TYPE,
            render_model_name: Property::PerHand {
                left: c"C:\\Users\\steamuser\\AppData\\Local\\Microsoft/Windows/OpenVR\\controller_1627_1118_1\\controller.obj",
                right: c"C:\\Users\\steamuser\\AppData\\Local\\Microsoft/Windows/OpenVR\\controller_1627_1118_2\\controller.obj",
            },
            main_axis: MainAxisType::Thumbstick,
            registered_device_type: Property::PerHand {
                left: c"WindowsMR: 0x045E/0x065B/0/1",
                right: c"WindowsMR: 0x045E/0x065B/0/2",
            },
            serial_number: wmr::SERIAL_NUMBER,
            tracking_system_name: wmr::TRACKING_SYSTEM_NAME,
            manufacturer_name: c"WindowsMR: 0x045E",
            input_profile_path: c"",
            legacy_buttons_mask: wmr::OG_LEGACY_BUTTONS_MASK,
            hmd: HmdProperties {
                model: c"WindowsMR",
                controller_type: c"holographic_controller",
                serial_number: c"MRSOURCE0",
                render_model_name: c"generic_hmd",
                registered_device_type: c"",
                input_profile_path: c"",
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
                component: Some(DynComponent::Value),
                ..
            } => Some(p.with_component(DynComponent::Click)),
            _ => None,
        }
    }

    fn legacy_bindings(c: &super::super::InputToXrPath<Self>) -> LegacyBindings {
        LegacyBindings {
            extra: legacy::Bindings {
                grip_pose: c.pose(),
            },
            app_menu: c.leftright::<Menu, Click, _, _>(),
            a: c.leftright::<Trackpad, Click, _, _>(),
            trigger: c.leftright::<Trigger, Value, _, _>(),
            trigger_click: c.leftright::<Trigger, Value, _, _>(),
            squeeze: c.leftright::<Squeeze, Click, _, _>(),
            squeeze_click: c.leftright::<Squeeze, Click, _, _>(),
            main_xy: c.leftright::<Thumbstick, (), _, _>(),
            main_xy_click: c.leftright::<Thumbstick, Click, _, _>(),
            main_xy_touch: vec![],
            app_menu_touch: vec![],
            a_touch: vec![],
            trigger_touch: vec![],
            axis2_touch: vec![],
            grip_touch: vec![],
            grip_click: vec![],
            main_xy_deflect: vec![],
            haptic: c.haptics(),
        }
    }

    fn skeletal_input_bindings(c: &super::super::InputToXrPath<Self>) -> SkeletalInputBindings {
        SkeletalInputBindings {
            thumb_touch: c.leftright::<Trackpad, Touch, _, _>(),
            index_touch: c.leftright::<Trigger, Value, _, _>(),
            index_curl: c.leftright::<Trigger, Value, _, _>(),
            rest_curl: c.leftright::<Squeeze, Click, _, _>(),
        }
    }

    fn offset_grip_pose(_hand: Hand) -> Mat4 {
        Mat4::from_translation(Vec3::new(0.0, 0.026310, -0.078693))
    }

    fn pose_transformation(_pose: BoundPoseType) -> Option<PoseTransformations> {
        None
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::input::tests::Fixture;

    pub(crate) fn base_verify_bindings(path: &str) {
        let f = Fixture::new();
        f.load_actions(c"actions.json");
        f.verify_bindings::<bool>(
            path,
            c"/actions/set1/in/boolact",
            [
                "/user/hand/left/input/thumbstick/click".into(),
                "/user/hand/right/input/thumbstick/click".into(),
                "/user/hand/left/input/squeeze/click".into(),
                "/user/hand/right/input/squeeze/click".into(),
                "/user/hand/left/input/menu/click".into(),
                "/user/hand/right/input/menu/click".into(),
                "/user/hand/left/input/trackpad/click".into(),
                "/user/hand/right/input/trackpad/click".into(),
                "/user/hand/left/input/trackpad/touch".into(),
                "/user/hand/right/input/trackpad/touch".into(),
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
        f.verify_bindings::<openxr::Vector2f>(
            path,
            c"/actions/set1/in/vec2act",
            [
                "/user/hand/left/input/trackpad".into(),
                "/user/hand/right/input/trackpad".into(),
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

    #[test]
    fn verify_bindings() {
        base_verify_bindings(HolographicController::profile_path());
    }
}
