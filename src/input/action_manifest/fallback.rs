//! Bindings for controllers a game has no bindings for, converted from another controller's
//! bindings like SteamVR does.

use super::actions::ControllerType;
use super::bindings::{Bindings, MaybeInputPath};
use crate::input::InteractionProfile;
use crate::input::profiles::{DynInputPath, RunWithProfile, paths::DynSubpath};
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
    pub fn known() -> [Self; 7] {
        [
            Self::ViveController,
            Self::ViveFocus3,
            Self::Knuckles,
            Self::FrameController,
            Self::OculusTouch,
            Self::HolographicController,
            Self::HPMotionController,
        ]
    }

    /// Controller types whose bindings convert best to this one, closest first.
    pub fn conversion_sources(&self) -> Vec<Self> {
        use ControllerType::*;
        let order = match self {
            // Controllers with a trackpad first.
            ViveController => [
                Knuckles,
                HolographicController,
                OculusTouch,
                ViveFocus3,
                HPMotionController,
                FrameController,
                ViveController,
            ],
            Knuckles => [
                OculusTouch,
                ViveFocus3,
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
                Knuckles,
                ViveFocus3,
                FrameController,
                ViveController,
            ],
            _ => [
                OculusTouch,
                Knuckles,
                ViveFocus3,
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
                    taken.push((input.hand, subpath));
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
    let hand = input.hand;
    let (lower_face, upper_face) = match hand {
        Hand::Left => ([X, A], [Y, B]),
        Hand::Right => ([A, X], [B, Y]),
    };
    let candidates = match input.subpath {
        axis if axis == source.main_axis() => vec![Thumbstick, Trackpad],
        // A second axis only has somewhere to go on controllers with two.
        Trackpad if target_has(hand, Thumbstick) => vec![Trackpad],
        Trackpad => vec![],
        A | X => lower_face.to_vec(),
        B | Y => upper_face.to_vec(),
        Menu => [Menu]
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
                hand,
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
}
