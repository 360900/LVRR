use super::{
    BoundPoseType, HmdProperties, InteractionProfile, MainAxisType, PoseTransformations,
    ProfileProperties, Property, SkeletalInputBindings, legal_paths, paths::*,
    vive_focus3::ViveFocus3,
};
use crate::button_mask_from_ids;
use crate::input::legacy::{self, LegacyBindings, button_mask_from_id};
use crate::input::profiles::DynInputPath;
use crate::openxr_data::Hand;
use glam::Mat4;
use openvr::EVRButtonId as btn;

/// The Vive Cosmos controllers (`XR_HTC_vive_cosmos_controller_interaction`). SteamVR's Cosmos
/// driver isn't part of SteamVR, so only the controller type and the headset's (which games
/// list bindings for) are known.
pub struct ViveCosmos;

impl InteractionProfile for ViveCosmos {
    const SIMULATE_CONVERTED: bool = true;
    type LegalPaths = legal_paths![
        Both::<
            (Squeeze, Click),
            (Trigger, Value),
            (Trigger, Click),
            (Thumbstick, ()),
            (Thumbstick, Click),
            (Thumbstick, Touch),
            (Shoulder, Click),
        >,
        Left::<(X, Click), (Y, Click), (Menu, Click)>,
        Right::<(A, Click), (B, Click), (System, Click)>
    ];

    fn properties() -> &'static ProfileProperties {
        static DEVICE_PROPERTIES: ProfileProperties = ProfileProperties {
            model: Property::BothHands(c"vive_cosmos_controller"),
            openvr_controller_type: c"vive_cosmos_controller",
            render_model_name: Property::PerHand {
                left: c"vive_cosmos_controller_left",
                right: c"vive_cosmos_controller_right",
            },
            registered_device_type: Property::BothHands(c"htc/vive_cosmos_controller"),
            serial_number: Property::PerHand {
                left: c"CTL_LEFT",
                right: c"CTL_RIGHT",
            },
            tracking_system_name: c"htc_eyes",
            manufacturer_name: c"HTC",
            input_profile_path: c"{htc}/input/vive_cosmos_controller_profile.json",
            main_axis: MainAxisType::Thumbstick,
            legacy_buttons_mask: button_mask_from_ids!(
                btn::System,
                btn::ApplicationMenu,
                btn::Grip,
                btn::A,
                btn::Axis0,
                btn::Axis1,
                btn::Axis2,
            ),
            // TODO: These are guesses, not pulled from a real system report, except the
            // headset's controller type, which games list bindings for.
            hmd: HmdProperties {
                model: c"VIVE Cosmos",
                controller_type: c"vive_cosmos",
                serial_number: c"HMD",
            },
        };
        &DEVICE_PROPERTIES
    }
    fn profile_path() -> &'static str {
        "/interaction_profiles/htc/vive_cosmos_controller"
    }
    fn has_required_extensions(enabled_extensions: &openxr::ExtensionSet) -> bool {
        enabled_extensions.htc_vive_cosmos_controller_interaction
    }
    fn translate_path(path: DynInputPath) -> Option<DynInputPath> {
        match path {
            // The buttons and grip only click.
            path @ DynInputPath {
                subpath: DynSubpath::A | DynSubpath::B | DynSubpath::X | DynSubpath::Y,
                component: Some(DynComponent::Touch),
                ..
            }
            | path @ DynInputPath {
                subpath: DynSubpath::Squeeze,
                component: Some(DynComponent::Value),
                ..
            } => Some(path.with_component(DynComponent::Click)),
            DynInputPath {
                device,
                subpath: DynSubpath::Bumper,
                component: Some(DynComponent::Click),
            } => Some(DynInputPath {
                device,
                subpath: DynSubpath::Shoulder,
                component: Some(DynComponent::Click),
            }),
            _ => None,
        }
    }

    /// Like SteamVR's for the Focus 3, as there are none for the Cosmos here.
    fn legacy_bindings(c: &super::InputToXrPath<Self>) -> LegacyBindings {
        LegacyBindings {
            extra: legacy::Bindings {
                grip_pose: c.pose(),
            },
            trigger: c.leftright::<Trigger, Value, _, _>(),
            trigger_click: c.leftright::<Trigger, Click, _, _>(),
            app_menu: [
                c.into::<super::Left<Y, Click>, _>(),
                c.into::<super::Left<Menu, Click>, _>(),
                c.into::<super::Right<B, Click>, _>(),
            ]
            .concat(),
            a: [
                c.into::<super::Left<X, Click>, _>(),
                c.into::<super::Right<A, Click>, _>(),
            ]
            .concat(),
            squeeze_click: c.leftright::<Squeeze, Click, _, _>(),
            squeeze: c.leftright::<Squeeze, Click, _, _>(),
            main_xy: c.leftright::<Thumbstick, (), _, _>(),
            main_xy_click: c.leftright::<Thumbstick, Click, _, _>(),
            main_xy_touch: c.leftright::<Thumbstick, Touch, _, _>(),
            trigger_touch: vec![],
            app_menu_touch: vec![],
            a_touch: vec![],
            axis2_touch: vec![],
            grip_touch: vec![],
            grip_click: vec![],
            main_xy_deflect: vec![],
            haptic: c.haptics(),
        }
    }

    fn skeletal_input_bindings(c: &super::InputToXrPath<Self>) -> SkeletalInputBindings {
        SkeletalInputBindings {
            thumb_touch: [
                c.leftright::<Thumbstick, Touch, _, _>(),
                c.into::<super::Left<X, Click>, _>(),
                c.into::<super::Left<Y, Click>, _>(),
                c.into::<super::Right<A, Click>, _>(),
                c.into::<super::Right<B, Click>, _>(),
            ]
            .concat(),
            index_touch: c.leftright::<Trigger, Click, _, _>(),
            index_curl: c.leftright::<Trigger, Value, _, _>(),
            rest_curl: c.leftright::<Squeeze, Click, _, _>(),
        }
    }

    fn offset_grip_pose(hand: Hand) -> Mat4 {
        ViveFocus3::offset_grip_pose(hand)
    }

    /// The Focus 3's, HTC's next controller, without SteamVR's Cosmos models to derive them
    /// from.
    fn pose_transformation(pose: BoundPoseType) -> Option<PoseTransformations> {
        ViveFocus3::pose_transformation(pose)
    }
}

#[cfg(test)]
mod tests {
    use super::{InteractionProfile, ViveCosmos};
    use crate::input::tests::Fixture;
    use openxr as xr;

    #[test]
    fn verify_bindings() {
        let f = Fixture::new();
        f.load_actions(c"actions.json");

        let path = ViveCosmos::profile_path();
        f.verify_bindings::<bool>(
            path,
            c"/actions/set1/in/boolact",
            [
                "/user/hand/left/input/x/click".into(),
                "/user/hand/left/input/y/click".into(),
                "/user/hand/right/input/a/click".into(),
                "/user/hand/right/input/b/click".into(),
                "/user/hand/left/input/shoulder/click".into(),
                "/user/hand/right/input/squeeze/click".into(),
                "/user/hand/left/input/menu/click".into(),
                "/user/hand/right/input/thumbstick/click".into(),
                "/user/hand/right/input/thumbstick/touch".into(),
            ],
        );
        f.verify_threshold_bindings(
            path,
            c"/actions/set1/in/boolact",
            ["/user/hand/right/input/trigger/value".into()],
        );

        // The buttons only click.
        f.verify_bindings::<bool>(
            path,
            c"/actions/set1/in/boolact2",
            ["/user/hand/left/input/x/click".into()],
        );

        // So does the grip, even when bound like an analog one.
        f.verify_bindings::<f32>(
            path,
            c"/actions/set1/in/vec1act",
            [
                "/user/hand/left/input/trigger/value".into(),
                "/user/hand/right/input/trigger/value".into(),
                "/user/hand/left/input/squeeze/click".into(),
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
