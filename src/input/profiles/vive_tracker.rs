use super::{
    BoundPoseType, HmdProperties, InteractionProfile, MainAxisType, PoseTransformations,
    ProfileProperties, Property, SkeletalInputBindings,
};
use crate::input::legacy::{Bindings, LegacyBindings};
use crate::openxr_data::Hand;
use glam::Mat4;

pub struct ViveTracker;

impl InteractionProfile for ViveTracker {
    type LegalPaths = ();
    fn profile_path() -> &'static str {
        "/interaction_profiles/htc/vive_tracker_htcx"
    }
    fn has_required_extensions(_: &openxr::ExtensionSet) -> bool {
        true
    }
    fn properties() -> &'static ProfileProperties {
        static DEVICE_PROPERTIES: ProfileProperties = ProfileProperties {
            model: Property::BothHands(c"Vive Tracker Handheld Object"),
            openvr_controller_type: c"vive_tracker_handheld_object",
            render_model_name: Property::BothHands(c"vr_tracker_vive_3_0"),
            main_axis: MainAxisType::Thumbstick,
            registered_device_type: Property::BothHands(c"vive_tracker"),
            serial_number: Property::BothHands(c"vive_tracker"), // This gets replaced
            tracking_system_name: c"lighthouse",
            manufacturer_name: c"HTC",
            input_profile_path: c"{htc}/input/vive_tracker_profile.json",
            legacy_buttons_mask: 0u64, // This is the closest thing I could think of to NOOP this
            hmd: HmdProperties {
                model: c"Vive. MV",
                controller_type: c"vive",
                serial_number: c"LHR-00000000",
            },
        };

        &DEVICE_PROPERTIES
    }

    fn legacy_bindings(c: &super::InputToXrPath<Self>) -> LegacyBindings {
        LegacyBindings {
            app_menu: vec![],
            a: vec![],
            trigger_click: vec![],
            squeeze_click: vec![],
            trigger: vec![],
            squeeze: vec![],
            main_xy: vec![],
            main_xy_touch: vec![],
            main_xy_click: vec![],
            haptic: vec![],
            extra: Bindings {
                grip_pose: c.pose(),
            },
        }
    }

    fn skeletal_input_bindings(_: &super::InputToXrPath<Self>) -> SkeletalInputBindings {
        SkeletalInputBindings {
            thumb_touch: Vec::new(),
            index_touch: Vec::new(),
            index_curl: Vec::new(),
            rest_curl: Vec::new(),
        }
    }

    fn offset_grip_pose(_: Hand) -> Mat4 {
        Mat4::IDENTITY
    }

    fn pose_transformation(_pose: BoundPoseType) -> Option<PoseTransformations> {
        None
    }
}
