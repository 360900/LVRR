//! Bindings for controllers a game has no bindings for, converted from another controller's
//! bindings like SteamVR does.

use super::actions::ControllerType;
use super::bindings::{Bindings, MaybeInputPath};
use crate::input::InteractionProfile;
use crate::input::profiles::{DynInputPath, InputDevice, RunWithProfile, paths::DynSubpath};
use crate::openxr_data::Hand;

impl ControllerType {
    /// Whether this type's bindings are loaded for `P`.
    pub fn covers<P: InteractionProfile>(&self) -> bool {
        struct Finder(&'static str, bool);
        impl RunWithProfile for Finder {
            fn run<Q: InteractionProfile>(&mut self) {
                self.1 |= Q::profile_path() == self.0;
            }
        }
        let mut finder = Finder(P::profile_path(), false);
        self.run_for_profile(&mut finder);
        finder.1
    }

    /// Every controller type a game can list bindings for.
    pub fn known() -> [Self; 9] {
        [
            Self::ViveController,
            Self::ViveFocus3,
            Self::Knuckles,
            Self::FrameController,
            Self::OculusTouch,
            Self::HolographicController,
            Self::HPMotionController,
            Self::ViveCosmosController,
            Self::PicoController,
        ]
    }

    /// Controller types whose bindings convert best to this one, closest first.
    ///
    /// Pico bindings come right after the Touch ones, or first for the Touch, as in SteamVR's
    /// remapping files. SteamVR never converts the Cosmos ones, which go after the Focus 3's.
    pub fn conversion_sources(&self) -> Vec<Self> {
        use ControllerType::*;
        let order = match self {
            // Controllers with a trackpad first.
            ViveController => [
                Knuckles,
                HolographicController,
                PicoController,
                OculusTouch,
                ViveFocus3,
                ViveCosmosController,
                HPMotionController,
                FrameController,
                ViveController,
            ],
            Knuckles => [
                OculusTouch,
                PicoController,
                ViveFocus3,
                ViveCosmosController,
                HPMotionController,
                FrameController,
                HolographicController,
                ViveController,
                Knuckles,
            ],
            HolographicController | HPMotionController => [
                HPMotionController,
                HolographicController,
                OculusTouch,
                PicoController,
                Knuckles,
                ViveFocus3,
                ViveCosmosController,
                FrameController,
                ViveController,
            ],
            _ => [
                OculusTouch,
                PicoController,
                Knuckles,
                ViveFocus3,
                ViveCosmosController,
                HPMotionController,
                HolographicController,
                FrameController,
                ViveController,
            ],
        };
        order.into_iter().filter(|other| other != self).collect()
    }

    /// The axis the controller's bindings use for movement and the like.
    fn main_axis(&self) -> DynSubpath {
        match self {
            Self::ViveController => DynSubpath::Trackpad,
            _ => DynSubpath::Thumbstick,
        }
    }
}

/// Moves `bindings`, made for `source`, to the inputs of a controller that has the inputs
/// `target_has` reports, by the role of each input: trigger to trigger, main axis to main axis,
/// face buttons to face buttons and so on. Bindings on inputs without an equivalent are dropped.
pub fn convert(
    bindings: &mut Bindings,
    source: &ControllerType,
    target_has: impl Fn(Hand, DynSubpath) -> bool,
) {
    let mut taken = Vec::new();
    // The menu button goes last, as it may move to a face button nothing else uses.
    for menu in [false, true] {
        for path in bindings.input_paths_mut() {
            let MaybeInputPath::Valid(input) = path else {
                continue;
            };
            if (input.subpath == DynSubpath::Menu) != menu {
                continue;
            }
            match convert_input(*input, source, &taken, &target_has) {
                Some(subpath) => {
                    input.subpath = subpath;
                    // Every menu binding on a hand goes to the same button, wherever it is.
                    if let (false, InputDevice::Hand(hand)) = (menu, input.device) {
                        taken.push((hand, subpath));
                    }
                }
                None => {
                    *path = MaybeInputPath::Invalid {
                        path: input.to_string(),
                        error: "no equivalent on this controller".into(),
                    }
                }
            }
        }
    }
}

fn convert_input(
    input: DynInputPath,
    source: &ControllerType,
    taken: &[(Hand, DynSubpath)],
    target_has: &impl Fn(Hand, DynSubpath) -> bool,
) -> Option<DynSubpath> {
    use DynSubpath::*;
    // Only hand controllers' bindings convert.
    let InputDevice::Hand(hand) = input.device else {
        return None;
    };
    // Without face buttons (the Frame's left hand), the d-pad stands in for them, like in
    // SteamVR's legacy bindings.
    let (lower_face, upper_face) = match hand {
        Hand::Left => ([X, A, DpadDown], [Y, B, DpadUp]),
        Hand::Right => ([A, X, DpadDown], [B, Y, DpadUp]),
    };
    let candidates = match input.subpath {
        axis if axis == source.main_axis() => vec![Thumbstick, Trackpad],
        // A second axis only has somewhere to go on controllers with two.
        Trackpad if target_has(hand, Thumbstick) => vec![Trackpad],
        Trackpad => vec![],
        A | X => lower_face.to_vec(),
        B | Y => upper_face.to_vec(),
        Menu => [Menu, View]
            .into_iter()
            .chain(
                upper_face
                    .into_iter()
                    .filter(|button| !taken.contains(&(hand, *button))),
            )
            .collect(),
        other => vec![other],
    };
    candidates
        .into_iter()
        .find(|subpath| target_has(hand, *subpath))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_roles() {
        // A Touch-like target: thumbstick, face buttons, a menu button only on the left.
        let touch = |hand: Hand, subpath: DynSubpath| {
            use DynSubpath::*;
            matches!(subpath, Trigger | Squeeze | Thumbstick)
                || match hand {
                    Hand::Left => matches!(subpath, X | Y | Menu),
                    Hand::Right => matches!(subpath, A | B | System),
                }
        };
        let vive = ControllerType::ViveController;
        let convert = |subpath, hand, source: &ControllerType, taken: &[(Hand, DynSubpath)]| {
            let input = DynInputPath {
                device: InputDevice::Hand(hand),
                subpath,
                component: None,
            };
            convert_input(input, source, taken, &touch)
        };
        use DynSubpath::*;
        assert_eq!(convert(Trackpad, Hand::Left, &vive, &[]), Some(Thumbstick));
        assert_eq!(convert(Menu, Hand::Left, &vive, &[]), Some(Menu));
        assert_eq!(convert(Menu, Hand::Right, &vive, &[]), Some(B));
        assert_eq!(convert(Menu, Hand::Right, &vive, &[(Hand::Right, B)]), None);
        let knuckles = ControllerType::Knuckles;
        assert_eq!(convert(A, Hand::Left, &knuckles, &[]), Some(X));
        assert_eq!(convert(B, Hand::Left, &knuckles, &[]), Some(Y));
        assert_eq!(convert(A, Hand::Right, &knuckles, &[]), Some(A));
        // Touch has no second axis for the Index trackpad.
        assert_eq!(convert(Trackpad, Hand::Right, &knuckles, &[]), None);
    }

    #[test]
    fn frame_controller_left_hand() {
        use crate::input::profiles::frame_controller::FrameController;
        use DynSubpath::*;
        let convert = |subpath, hand, source: &ControllerType| {
            let input = DynInputPath {
                device: InputDevice::Hand(hand),
                subpath,
                component: None,
            };
            convert_input(
                input,
                source,
                &[],
                &super::super::has_input::<FrameController>,
            )
        };
        let touch = ControllerType::OculusTouch;
        // The left hand has a d-pad and a view button instead of face buttons and a menu.
        assert_eq!(convert(X, Hand::Left, &touch), Some(DpadDown));
        assert_eq!(convert(Y, Hand::Left, &touch), Some(DpadUp));
        assert_eq!(convert(Menu, Hand::Left, &touch), Some(View));
        assert_eq!(convert(A, Hand::Right, &touch), Some(A));
        assert_eq!(convert(B, Hand::Right, &touch), Some(B));
        let knuckles = ControllerType::Knuckles;
        assert_eq!(convert(A, Hand::Left, &knuckles), Some(DpadDown));
        assert_eq!(convert(B, Hand::Left, &knuckles), Some(DpadUp));
        let vive = ControllerType::ViveController;
        assert_eq!(convert(Menu, Hand::Right, &vive), Some(Menu));
        assert_eq!(convert(Trackpad, Hand::Left, &vive), Some(Thumbstick));
    }

    #[test]
    fn trackpad_controllers() {
        use crate::input::profiles::{
            vive_controller::ViveWands, wmr::ms_motion_controller::HolographicController,
        };
        use DynSubpath::*;
        let convert = |subpath, hand, source: &ControllerType, target_has: &dyn Fn(_, _) -> _| {
            let input = DynInputPath {
                device: InputDevice::Hand(hand),
                subpath,
                component: None,
            };
            convert_input(input, source, &[], &target_has)
        };
        let vive = &super::super::has_input::<ViveWands>;
        let wmr = &super::super::has_input::<HolographicController>;
        let touch = ControllerType::OculusTouch;
        // The stick moves to the Vive trackpad, and the face buttons have nowhere to go.
        assert_eq!(
            convert(Thumbstick, Hand::Left, &touch, vive),
            Some(Trackpad)
        );
        assert_eq!(convert(X, Hand::Left, &touch, vive), None);
        assert_eq!(convert(Menu, Hand::Left, &touch, vive), Some(Menu));
        assert_eq!(convert(Squeeze, Hand::Right, &touch, vive), Some(Squeeze));
        assert_eq!(convert(Trigger, Hand::Right, &touch, vive), Some(Trigger));
        // WMR controllers have both axes.
        let knuckles = ControllerType::Knuckles;
        assert_eq!(
            convert(Thumbstick, Hand::Right, &knuckles, wmr),
            Some(Thumbstick)
        );
        assert_eq!(
            convert(Trackpad, Hand::Right, &knuckles, wmr),
            Some(Trackpad)
        );
        assert_eq!(convert(B, Hand::Right, &knuckles, wmr), None);
        let vive = ControllerType::ViveController;
        assert_eq!(convert(Trackpad, Hand::Left, &vive, wmr), Some(Thumbstick));
    }

    #[test]
    fn conversion_sources() {
        use ControllerType::*;
        // Like SteamVR's remapping files: Pico right after the Touch, or first for it.
        assert_eq!(
            OculusTouch.conversion_sources()[..2],
            [PicoController, Knuckles]
        );
        assert_eq!(
            PicoController.conversion_sources()[..2],
            [OculusTouch, Knuckles]
        );
        assert_eq!(
            ViveCosmosController.conversion_sources()[..3],
            [OculusTouch, PicoController, Knuckles]
        );
        assert_eq!(
            Knuckles.conversion_sources()[..2],
            [OculusTouch, PicoController]
        );
        assert_eq!(
            ViveController.conversion_sources()[2..4],
            [PicoController, OculusTouch]
        );

        let known = ControllerType::known();
        for ty in &known {
            let sources = ty.conversion_sources();
            assert_eq!(sources.len(), known.len() - 1, "{ty}");
            assert!(
                known
                    .iter()
                    .all(|other| other == ty || sources.contains(other)),
                "{ty}"
            );
            // The names games list them under.
            assert_eq!(ty.to_string().parse::<ControllerType>(), Ok(ty.clone()));
            let json: ControllerType = serde_json::from_str(&format!("\"{ty}\"")).unwrap();
            assert_eq!(&json, ty);
        }
    }
}
