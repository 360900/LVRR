use super::{
    BoundPoseType, HmdProperties, InteractionProfile, MainAxisType, PoseTransformations,
    ProfileProperties, Property, SkeletalInputBindings, legal_paths, paths::*,
};
use crate::button_mask_from_ids;
use crate::input::legacy::{Bindings, LegacyBindings, button_mask_from_id};
use crate::openxr_data::Hand;
use glam::Mat4;
use openvr::EVRButtonId;

/// Synthetic interaction profile reported when a controller has no bound
/// interaction profile but is tracked through hand tracking (e.g. WiVRn).
pub struct VRLinkHand;

impl InteractionProfile for VRLinkHand {
    type LegalPaths = legal_paths![Both::<(Select, Click)>];

    fn profile_path() -> &'static str {
        "/interaction_profiles/ext/hand_interaction_ext"
    }

    fn has_required_extensions(enabled_extensions: &openxr::ExtensionSet) -> bool {
        enabled_extensions.ext_hand_tracking
    }

    fn properties() -> &'static ProfileProperties {
        static DEVICE_PROPERTIES: ProfileProperties = ProfileProperties {
            model: Property::PerHand {
                left: c"VRLink Hand Tracker (Left Hand)",
                right: c"VRLink Hand Tracker (Right Hand)",
            },
            openvr_controller_type: c"svl_hand_interaction_augmented",
            render_model_name: Property::BothHands(c"{vrlink}/rendermodels/shuttlecock"),
            main_axis: MainAxisType::Thumbstick,
            registered_device_type: Property::PerHand {
                left: c"vrlink/VRLINKQ_HandTracker_Left",
                right: c"vrlink/VRLINKQ_HandTracker_Right",
            },
            serial_number: Property::PerHand {
                left: c"VRLINKQ_Hand_Left",
                right: c"VRLINKQ_Hand_Right",
            },
            tracking_system_name: c"vrlink",
            manufacturer_name: c"VRLink",
            input_profile_path: c"{vrlink}/input/svl_hand_interaction_augmented_input_profile.json",
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
                model: c"VRLink",
                controller_type: c"vrlink",
                serial_number: c"VRLINKQ_HMD",
            },
        };
        &DEVICE_PROPERTIES
    }

    fn legacy_bindings(c: &super::InputToXrPath<Self>) -> LegacyBindings {
        LegacyBindings {
            extra: Bindings {
                grip_pose: c.pose(),
            },
            trigger: c.leftright::<Select, Click, _, _>(),
            trigger_click: c.leftright::<Select, Click, _, _>(),
            app_menu: vec![],
            a: vec![],
            squeeze: vec![],
            squeeze_click: vec![],
            main_xy: vec![],
            main_xy_click: vec![],
            main_xy_touch: vec![],
            haptic: c.haptics(),
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
