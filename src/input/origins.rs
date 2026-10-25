//! What SteamVR reports about the inputs actions are bound to: an input's OpenVR path on a
//! controller, the names GetOriginLocalizedName gives origins and the render model components
//! GetOriginTrackedDeviceInfo names.

use crate::input::profiles::{InputDevice, paths::DynSubpath};
use crate::openxr_data::Hand;

/// A binding as GetActionBindingInfo reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingInfo {
    pub device: InputDevice,
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
    InputDevice::Hand(hand).path()
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
        // OpenXR's name for the Cosmos bumpers.
        DynSubpath::Shoulder => "bumper",
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

struct Driver {
    controller_type: &'static str,
    name: &'static str,
    inputs: &'static [(&'static str, &'static str)],
}

/// The English names drivers give their controllers and inputs, from
/// `drivers/*/resources/localization/localization.json` of SteamVR.
const DRIVERS: &[Driver] = &[
    Driver {
        controller_type: "knuckles",
        name: "Index Controller",
        inputs: &[
            ("/input/a", "A Button"),
            ("/input/b", "B Button"),
            ("/input/pinch", "Pinch Gesture"),
            ("/input/finger/index", "Index Finger"),
            ("/input/finger/middle", "Middle Finger"),
            ("/input/finger/ring", "Ring Finger"),
            ("/input/finger/pinky", "Pinky Finger"),
            ("/input/thumbstick", "Thumb Stick"),
            ("/input/trigger", "Trigger"),
            ("/input/trackpad", "Trackpad"),
            ("/input/grip", "Grip"),
        ],
    },
    Driver {
        controller_type: "oculus_touch",
        name: "Oculus Touch",
        inputs: &[
            ("/input/a", "A Button"),
            ("/input/b", "B Button"),
            ("/input/x", "X Button"),
            ("/input/y", "Y Button"),
            ("/input/joystick", "Joystick"),
            ("/input/thumbrest", "Thumbrest"),
        ],
    },
    Driver {
        controller_type: "vive_controller",
        name: "Vive Controller",
        inputs: &[("/input/trigger", "Trigger")],
    },
    Driver {
        controller_type: "vive",
        name: "Vive",
        inputs: &[],
    },
    Driver {
        controller_type: "frame_controller",
        name: "Frame Controller",
        inputs: &[
            ("/input/thumbrest", "Thumbrest"),
            ("/input/thumbstick", "Thumb Stick"),
            ("/input/system", "Steam Button"),
            ("/input/menu", "Menu Button"),
            ("/input/view", "View Button"),
            ("/input/a", "A Button"),
            ("/input/b", "B Button"),
            ("/input/x", "X Button"),
            ("/input/y", "Y Button"),
            ("/input/bumper", "Bumper"),
            ("/input/trigger", "Trigger"),
            ("/input/grip", "Grip"),
            ("/input/dpad", "Dpad"),
            ("/input/dpad_up", "Dpad Up"),
            ("/input/dpad_right", "Dpad Right"),
            ("/input/dpad_down", "Dpad Down"),
            ("/input/dpad_left", "Dpad Left"),
        ],
    },
    Driver {
        controller_type: "svl_hand_interaction_augmented",
        name: "Hand Tracking Controller",
        inputs: &[
            ("/input/index_pinch", "Index Pinch"),
            ("/input/middle_pinch", "Middle Pinch"),
            ("/input/ring_pinch", "Ring Pinch"),
            ("/input/pinky_pinch", "Pinky Pinch"),
            ("/input/index_point", "Index Point"),
        ],
    },
];

/// Names of controller types whose drivers aren't part of SteamVR, from its binding UI
/// (`resources/webinterface/dashboard/localization/bindingui_english.json`).
const OTHER_CONTROLLER_TYPES: &[(&str, &str)] = &[
    ("holographic_controller", "Windows Mixed Reality Controller"),
    ("holographic_hmd", "Windows Mixed Reality Headset"),
    ("indexhmd", "Index Headset"),
    ("rift", "Oculus Headset"),
    ("pico_controller", "Pico Controller"),
    // Not named in SteamVR's files.
    ("hpmotioncontroller", "HP Motion Controller"),
    ("vive_focus3_controller", "Vive Focus 3 Controller"),
    ("vive_cosmos_controller", "Vive Cosmos Controller"),
];

/// Names of inputs no driver names, from SteamVR's
/// `resources/webinterface/dashboard/localization/webhelper_english.json`, the strings vrclient
/// formats origin names with.
const INPUTS: &[(&str, &str)] = &[
    ("/input/joystick", "Joystick"),
    ("/input/thumbstick", "Thumbstick"),
    ("/input/trackpad", "Trackpad"),
    ("/input/trigger", "Trigger"),
    ("/input/grip", "Grip"),
    ("/input/application_menu", "Menu"),
    ("/input/system", "System"),
    ("/proximity", "Proximity Sensor"),
    ("/input/a", "A Button"),
    ("/input/b", "B Button"),
    ("/input/x", "X Button"),
    ("/input/y", "Y Button"),
    ("/input/dpad_up", "Dpad Up Button"),
    ("/input/dpad_down", "Dpad Down Button"),
    ("/input/dpad_left", "Dpad Left Button"),
    ("/input/dpad_right", "Dpad Right Button"),
    ("/input/bumper", "Bumper Button"),
    ("/input/menu", "Menu Button"),
    ("/input/view", "View Button"),
    ("/pose/raw", "Raw"),
    ("/pose/base", "Base"),
    ("/pose/tip", "Tip"),
    ("/pose/gdc2015", "GDC"),
    ("/pose/handgrip", "Hand Grip"),
    ("/pose/grip", "Grip"),
    ("/pose/openxr_grip", "OpenXR Grip"),
    ("/pose/openxr_aim", "OpenXR Aim"),
    ("/pose/openxr_poke", "Poke"),
    ("/pose/openxr_pinch", "Pinch"),
    ("/pose/openxr_handmodel", "Palm"),
    ("/input/skeleton/left", "Left skeleton"),
    ("/input/skeleton/right", "Right skeleton"),
    ("/output/haptic", "Haptics"),
];

/// The name of a hand, from webhelper_english.json.
pub fn hand_name(hand: Hand) -> &'static str {
    match hand {
        Hand::Left => "Left Hand",
        Hand::Right => "Right Hand",
    }
}

/// The name of a controller (or headset) type, e.g. "Index Controller" for `knuckles`.
pub fn controller_name(controller_type: &str) -> String {
    DRIVERS
        .iter()
        .find(|driver| driver.controller_type == controller_type)
        .map(|driver| driver.name)
        .or_else(|| lookup(OTHER_CONTROLLER_TYPES, controller_type))
        .map(str::to_owned)
        .unwrap_or_else(|| match readable(controller_type) {
            name if name.is_empty() => "Controller".to_owned(),
            name => name,
        })
}

/// The name of an input of a controller type, e.g. "Thumb Stick" for `/input/thumbstick` on a
/// `knuckles`. `input` is the part of an input source path after the device, with or without
/// the component (`/input/trigger/click`). Inputs SteamVR has no name for get one made from
/// the path.
pub fn input_name(controller_type: Option<&str>, input: &str) -> String {
    let driver = controller_type
        .and_then(|ty| DRIVERS.iter().find(|driver| driver.controller_type == ty))
        .map_or(&[][..], |driver| driver.inputs);
    let input = openvr_spelling(input);
    let without_component = strip_component(&input);
    [input.as_str(), without_component]
        .into_iter()
        .find_map(|path| lookup(driver, path).or_else(|| lookup(INPUTS, path)))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let name = without_component
                .strip_prefix("/input/")
                .or_else(|| without_component.strip_prefix("/output/"))
                .or_else(|| without_component.strip_prefix("/pose/"))
                .unwrap_or(without_component);
            readable(name)
        })
}

/// `input` with OpenXR's names for inputs replaced by OpenVR's.
fn openvr_spelling(input: &str) -> String {
    match input.strip_prefix("/input/squeeze") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("/input/grip{rest}"),
        _ => input.to_owned(),
    }
}

/// `input` without a final component like `/click`, unless that's all there is (`/input/x`).
fn strip_component(input: &str) -> &str {
    const COMPONENTS: &[&str] = &[
        "click", "touch", "value", "force", "pull", "x", "y", "position",
    ];
    match input.rsplit_once('/') {
        Some((rest, last)) if COMPONENTS.contains(&last) && rest.matches('/').count() >= 2 => rest,
        _ => input,
    }
}

fn lookup(table: &[(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// "Finger Index" for `finger/index`: words split on `/` and `_`, capitalized. Empty for names
/// that aren't plain words, like `<unknown>`.
fn readable(name: &str) -> String {
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '/' || c == ' ')
    {
        return String::new();
    }
    name.split(['/', '_', ' '])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect::<String>()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The component of SteamVR's render model `render_model` that shows `input` (an input name
/// like `joystick`): the first one whose motion follows that input, in
/// `resources/rendermodels/*/*.json` and `drivers/*/resources/rendermodels/*/*.json` of SteamVR.
pub fn render_model_component(render_model: &str, input: &str) -> Option<&'static str> {
    const QUEST: &[(&str, &str)] = &[
        ("trigger", "trigger"),
        ("grip", "button_grip"),
        ("joystick", "thumbstick"),
        ("thumbstick", "thumbstick"),
        ("a", "button_a"),
        ("b", "button_b"),
        ("x", "button_x"),
        ("y", "button_y"),
        ("system", "button_enter"),
        ("menu", "button_enter"),
    ];
    const INDEX: &[(&str, &str)] = &[
        ("trigger", "trigger"),
        ("trackpad", "trackpad"),
        ("grip", "squeeze"),
        ("thumbstick", "thumbstick"),
        ("joystick", "thumbstick"),
        ("system", "sys_button"),
        ("a", "button_a"),
        ("b", "button_b"),
    ];
    const VIVE: &[(&str, &str)] = &[
        ("trigger", "trigger"),
        ("trackpad", "trackpad"),
        ("grip", "lgrip"),
        ("system", "sys_button"),
        ("application_menu", "button"),
        ("menu", "button"),
    ];
    const FOCUS3_LEFT: &[(&str, &str)] = &[
        ("trigger", "trigger"),
        ("joystick", "joystick"),
        ("thumbstick", "joystick"),
        ("grip", "lgrip"),
        ("x", "button"),
        ("y", "buttonB"),
        ("system", "buttonC"),
        ("menu", "buttonC"),
    ];
    const FOCUS3_RIGHT: &[(&str, &str)] = &[
        ("trigger", "trigger"),
        ("joystick", "joystick"),
        ("thumbstick", "joystick"),
        ("grip", "rgrip"),
        ("a", "button"),
        ("b", "buttonB"),
        ("system", "buttonC"),
    ];
    const FRAME: &[(&str, &str)] = &[
        ("thumbstick", "thumbstick"),
        ("joystick", "thumbstick"),
        ("dpad_up", "dpad"),
        ("dpad_down", "dpad"),
        ("dpad_left", "dpad"),
        ("dpad_right", "dpad"),
        ("grip", "button_grip"),
        ("trigger", "trigger"),
        ("view", "button_view"),
        ("menu", "button_menu"),
        ("system", "button_system"),
        ("bumper", "bumper"),
        ("a", "button_a"),
        ("b", "button_b"),
        ("x", "button_x"),
        ("y", "button_y"),
    ];
    let table = [
        ("oculus_quest", QUEST),
        ("valve_controller_knu", INDEX),
        ("vr_controller_vive", VIVE),
        ("vive_focus3_controller_left", FOCUS3_LEFT),
        ("vive_focus3_controller_right", FOCUS3_RIGHT),
        ("frame_controller", FRAME),
    ]
    .into_iter()
    .find(|(model, _)| render_model.contains(model))?
    .1;
    let input = if input == "squeeze" { "grip" } else { input };
    lookup(table, input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_names() {
        for (controller_type, input, name) in [
            // Driver names win over the generic ones.
            (Some("knuckles"), "/input/thumbstick", "Thumb Stick"),
            (Some("oculus_touch"), "/input/thumbstick", "Thumbstick"),
            (Some("oculus_touch"), "/input/joystick", "Joystick"),
            (Some("frame_controller"), "/input/system", "Steam Button"),
            (Some("vive_controller"), "/input/system", "System"),
            (Some("vive_controller"), "/input/application_menu", "Menu"),
            (Some("knuckles"), "/input/finger/index", "Index Finger"),
            (None, "/input/a", "A Button"),
            // Components and OpenXR spellings.
            (Some("oculus_touch"), "/input/trigger/click", "Trigger"),
            (Some("oculus_touch"), "/input/squeeze/value", "Grip"),
            (Some("knuckles"), "/input/x", "X Button"),
            (Some("knuckles"), "/input/trackpad/x", "Trackpad"),
            (None, "/output/haptic", "Haptics"),
            (None, "/proximity", "Proximity Sensor"),
            // Unknown inputs get a name from the path.
            (Some("knuckles"), "/input/pinch_ext/value", "Pinch Ext"),
            (None, "/input/finger/thumb", "Finger Thumb"),
            (None, "/input/squeezer", "Squeezer"),
        ] {
            assert_eq!(input_name(controller_type, input), name, "{input}");
        }
    }

    #[test]
    fn controller_names() {
        for (controller_type, name) in [
            ("knuckles", "Index Controller"),
            ("oculus_touch", "Oculus Touch"),
            ("vive_controller", "Vive Controller"),
            ("frame_controller", "Frame Controller"),
            ("holographic_controller", "Windows Mixed Reality Controller"),
            ("hpmotioncontroller", "HP Motion Controller"),
            ("rift", "Oculus Headset"),
            ("some_new_controller", "Some New Controller"),
            ("<unknown>", "Controller"),
            ("", "Controller"),
        ] {
            assert_eq!(controller_name(controller_type), name, "{controller_type}");
        }
    }

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

    #[test]
    fn render_model_components() {
        for (model, input, component) in [
            (
                "oculus_quest2_controller_left",
                "joystick",
                Some("thumbstick"),
            ),
            ("oculus_quest_plus_controller_right", "a", Some("button_a")),
            ("oculus_quest2_controller_left", "grip", Some("button_grip")),
            (
                "{indexcontroller}valve_controller_knu_1_0_left",
                "grip",
                Some("squeeze"),
            ),
            (
                "{indexcontroller}valve_controller_knu_1_0_right",
                "squeeze",
                Some("squeeze"),
            ),
            ("vr_controller_vive_1_5", "application_menu", Some("button")),
            ("vive_focus3_controller_right", "grip", Some("rgrip")),
            (
                "{frame_controller}frame_controller_left",
                "dpad_up",
                Some("dpad"),
            ),
            ("generic_controller", "trigger", None),
            ("oculus_quest2_controller_left", "pinch", None),
        ] {
            assert_eq!(
                render_model_component(model, input),
                component,
                "{model} {input}"
            );
        }
    }
}
