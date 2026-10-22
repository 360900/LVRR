use super::super::{
    HmdProperties, InteractionProfile, MainAxisType, PoseTransformations, ProfileProperties,
    Property, SkeletalInputBindings, paths::*,
};
use super::ms_motion_controller::HolographicController;
use crate::input::BoundPoseType;
use crate::input::legacy::{self, LegacyBindings};
use crate::input::profiles::DynInputPath;
use crate::input::profiles::wmr;
use crate::openxr_data::Hand;
use glam::{Mat4, Vec3};
use openxr as xr;

pub struct SamsungOdysseyController;

impl InteractionProfile for SamsungOdysseyController {
    type LegalPaths = <HolographicController as InteractionProfile>::LegalPaths;

    fn profile_path() -> &'static str {
        "/interaction_profiles/samsung/odyssey_controller"
    }

    fn has_required_extensions(enabled_extensions: &xr::ExtensionSet) -> bool {
        enabled_extensions.ext_samsung_odyssey_controller
    }

    fn properties() -> &'static ProfileProperties {
        static DEVICE_PROPERTIES: ProfileProperties = ProfileProperties {
            model: Property::PerHand {
                left: c"WindowsMR: 0x04E8/0x065D/0/1",
                right: c"WindowsMR: 0x04E8/0x065D/0/2",
            },
            openvr_controller_type: wmr::OG_OPENVR_CONTROLLER_TYPE,
            render_model_name: Property::PerHand {
                left: c"C:\\Users\\steamuser\\AppData\\Local\\Microsoft/Windows/OpenVR\\controller_1629_1256_1\\controller.obj",
                right: c"C:\\Users\\steamuser\\AppData\\Local\\Microsoft/Windows/OpenVR\\controller_1629_1256_2\\controller.obj",
            },
            main_axis: MainAxisType::Thumbstick,
            registered_device_type: Property::PerHand {
                left: c"WindowsMR/MRSOURCE0",
                right: c"WindowsMR/MRSOURCE1",
            },
            serial_number: wmr::SERIAL_NUMBER,
            tracking_system_name: wmr::TRACKING_SYSTEM_NAME,
            manufacturer_name: c"WindowsMR: 0x04E8",
            input_profile_path: c"",
            legacy_buttons_mask: wmr::OG_LEGACY_BUTTONS_MASK,
            hmd: HmdProperties {
                model: c"Samsung Odyssey",
                controller_type: c"holographic_controller",
                serial_number: c"MRSOURCE0",
            },
        };
        &DEVICE_PROPERTIES
    }

    fn translate_path(path: DynInputPath) -> Option<DynInputPath> {
        HolographicController::translate_path(path)
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
            grip_touch: vec![],
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
        Mat4::from_translation(Vec3::new(0.0, 0.079738, -0.035449))
    }

    fn pose_transformation(_pose: BoundPoseType) -> Option<PoseTransformations> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_bindings() {
        super::super::ms_motion_controller::tests::base_verify_bindings(
            SamsungOdysseyController::profile_path(),
        );
    }
}
