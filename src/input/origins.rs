//! What SteamVR reports about the inputs actions are bound to: an input's OpenVR path on a
//! controller.

use crate::input::profiles::paths::DynSubpath;
use crate::openxr_data::Hand;

/// A binding as GetActionBindingInfo reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingInfo {
    pub hand: Hand,
    /// The input's OpenVR path on the controller, e.g. `/input/joystick`.
    pub input: String,
    /// The binding's mode, e.g. `dpad`. Empty for haptics.
    pub mode: &'static str,
    /// The input of the mode the action is bound to, e.g. `north`.
    pub slot: &'static str,
    /// The input's type in SteamVR's profile of the controller, e.g. `joystick`.
    pub source_type: &'static str,
}

pub fn hand_path(hand: Hand) -> &'static str {
    match hand {
        Hand::Left => "/user/hand/left",
        Hand::Right => "/user/hand/right",
    }
}

/// The OpenVR path of `subpath` on controllers of `controller_type`, as SteamVR's profiles of
/// them name it (`drivers/*/resources/input/*_profile.json`). Bindings converted from another
/// controller and OpenXR use other names.
pub fn openvr_input(controller_type: &str, subpath: DynSubpath) -> String {
    let name = match subpath {
        DynSubpath::Squeeze => "grip",
        // Only Valve's controllers call it a thumbstick.
        DynSubpath::Thumbstick if matches!(controller_type, "knuckles" | "frame_controller") => {
            "thumbstick"
        }
        DynSubpath::Thumbstick => "joystick",
        DynSubpath::Menu => match controller_type {
            "vive_controller" | "holographic_controller" | "hpmotioncontroller" => {
                "application_menu"
            }
            // The left menu button is the system button in SteamVR's profiles of these.
            "oculus_touch" | "vive_focus3_controller" | "pico_controller" => "system",
            _ => "menu",
        },
        other => return format!("/input/{other}"),
    };
    format!("/input/{name}")
}

/// The `type` SteamVR's controller profiles give an input.
pub fn source_type(subpath: DynSubpath, analog: bool) -> &'static str {
    match subpath {
        DynSubpath::Thumbstick => "joystick",
        DynSubpath::Trackpad => "trackpad",
        DynSubpath::Trigger | DynSubpath::PinchExt | DynSubpath::GraspExt => "trigger",
        // An analog grip is a trigger (Index, Touch), a clicky one a button (Vive).
        DynSubpath::Squeeze if analog => "trigger",
        _ => "button",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openvr_inputs() {
        for (controller_type, subpath, input) in [
            ("knuckles", DynSubpath::Thumbstick, "/input/thumbstick"),
            ("oculus_touch", DynSubpath::Thumbstick, "/input/joystick"),
            ("oculus_touch", DynSubpath::Squeeze, "/input/grip"),
            ("oculus_touch", DynSubpath::Menu, "/input/system"),
            (
                "vive_controller",
                DynSubpath::Menu,
                "/input/application_menu",
            ),
            ("frame_controller", DynSubpath::Menu, "/input/menu"),
            ("frame_controller", DynSubpath::DpadUp, "/input/dpad_up"),
            ("knuckles", DynSubpath::A, "/input/a"),
        ] {
            assert_eq!(openvr_input(controller_type, subpath), input);
        }
    }
}
