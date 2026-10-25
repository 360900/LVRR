pub mod frame_controller;
pub mod knuckles;
pub mod meta_touch_plus;
pub mod oculus_touch;
pub mod pico;
pub mod simple_controller;
pub mod touch_pro;
pub mod vive_controller;
pub mod vive_cosmos;
pub mod vive_focus3;
#[cfg(feature = "monado")]
pub mod vive_tracker;
pub mod vrlink_hand;
pub mod wmr;
pub mod xbox_controller;
use super::{
    BoundPoseType, action_manifest::ControllerType, legacy::LegacyBindings,
    skeletal::SkeletalInputBindings,
};
use crate::input::profiles::typemagic::ContainsPath;
use crate::openxr_data::Hand;
use frame_controller::FrameController;
use glam::Mat4;
use knuckles::Knuckles;
use meta_touch_plus::MetaTouchPlus;
use oculus_touch::OculusTouch;
use openxr as xr;
use pico::{Pico4, PicoNeo3};
use simple_controller::SimpleController;
use std::ffi::CStr;
use std::marker::PhantomData;
use touch_pro::TouchPro;
use vive_controller::ViveWands;
use vive_cosmos::ViveCosmos;
use vive_focus3::ViveFocus3;
use wmr::{
    hp_motion_controller::ReverbG2Controller, ms_motion_controller::HolographicController,
    samsung_odyssey_controller::SamsungOdysseyController,
};
use xbox_controller::XboxController;

#[allow(private_interfaces)]
pub trait InteractionProfile: SupportedProfile + Sized + 'static {
    const USE_FORCE_DPAD: bool = false;
    /// The skeletal tracking level SteamVR always reports for this controller, if it doesn't
    /// depend on what the runtime tracks.
    const SKELETAL_TRACKING_LEVEL: Option<openvr::EVRSkeletalTrackingLevel> = None;
    /// Whether the controller senses the fingers, so a hand skeleton the runtime derives from
    /// it is better than the estimated one.
    const SENSES_FINGERS: bool = false;
    /// The trigger values (activate, deactivate) at which SteamVR's legacy binding for this
    /// controller presses and releases k_EButton_SteamVR_Trigger, besides the trigger's click.
    const LEGACY_TRIGGER_CLICK: Option<(f32, f32)> = None;
    /// Likewise for k_EButton_Grip and the legacy squeeze value.
    const LEGACY_GRIP_CLICK: Option<(f32, f32)> = None;
    /// Whether a game whose bindings for this controller were converted from another
    /// controller's sees that controller, like SteamVR's remapping layouts do with
    /// `simulate_controller_type`, `simulate_render_model` and `simulate_HMD`. Only for profiles
    /// games rarely know, so far: the others keep reporting themselves.
    const SIMULATE_CONVERTED: bool = false;
    /// Whether this is the gamepad's profile (`/user/gamepad`) rather than a hand controller's.
    const GAMEPAD: bool = false;
    type LegalPaths: LegalPathsT;
    fn profile_path() -> &'static str;
    fn has_required_extensions(enabled_extensions: &xr::ExtensionSet) -> bool;
    fn properties() -> &'static ProfileProperties;
    fn translate_path(_path: DynInputPath) -> Option<DynInputPath> {
        None
    }
    fn legacy_bindings(converter: &InputToXrPath<Self>) -> LegacyBindings;
    fn skeletal_input_bindings(converter: &InputToXrPath<Self>) -> SkeletalInputBindings;
    /// Can be extracted from SteamVR rendermodel files, it is the inverse of the "grip" or "openxr_grip" value
    fn offset_grip_pose(_: Hand) -> Mat4;
    /// Can be extracted from SteamVR controller driver JSON, `component.{tip,base,etc}`.
    ///
    /// You can use the `resources/generate_pose_transforms.py` script to generate
    /// these for you.
    fn pose_transformation(pose: BoundPoseType) -> Option<PoseTransformations>;
}

/// Helper for generating `offset_grip_pose` methods via `pose_transformation`
/// & a `BoundPoseType`.
///
/// Looks up the proper matrix for the hand+type & inverts it, defaulting back
/// to the identity matrix.
fn offset_grip_pose_from_pose_type<P: InteractionProfile>(
    hand: Hand,
    pose_type: BoundPoseType,
) -> Mat4 {
    P::pose_transformation(pose_type)
        .map_or(Mat4::IDENTITY, |pose_transform| match hand {
            Hand::Left => pose_transform.left_hand,
            Hand::Right => pose_transform.right_hand,
        })
        .inverse()
}

pub(super) trait RunWithProfile {
    fn run<P: InteractionProfile>(&mut self);
    fn keep_running(&self) -> bool {
        true
    }
}

impl ControllerType {
    pub fn run_for_profile(&self, runner: &mut impl RunWithProfile) {
        // All profiles must be added here to have its actions loaded.
        match self {
            Self::ViveController => {
                runner.run::<ViveWands>();
                runner.run::<SimpleController>();
            }
            Self::OculusTouch => {
                runner.run::<MetaTouchPlus>();
                runner.run::<TouchPro>();
                runner.run::<OculusTouch>();
            }
            Self::Knuckles => runner.run::<Knuckles>(),
            Self::FrameController => runner.run::<FrameController>(),
            Self::ViveFocus3 => runner.run::<ViveFocus3>(),
            Self::HolographicController => {
                runner.run::<SamsungOdysseyController>();
                runner.run::<HolographicController>();
            }
            Self::HPMotionController => runner.run::<ReverbG2Controller>(),
            Self::ViveCosmosController => runner.run::<ViveCosmos>(),
            Self::PicoController => {
                runner.run::<Pico4>();
                runner.run::<PicoNeo3>();
            }
            Self::Gamepad => runner.run::<XboxController>(),
            Self::GenericHmd | Self::Unknown(_) => {}
        }
    }

    /// The identity SteamVR gives controllers that simulate this type, from the
    /// `simulation_settings` of its input profile, e.g. the Quest 2 for oculus_touch.
    pub fn simulated_properties(&self) -> Option<&'static ProfileProperties> {
        Some(match self {
            Self::ViveController => ViveWands::properties(),
            Self::OculusTouch => OculusTouch::properties(),
            Self::Knuckles => Knuckles::properties(),
            Self::FrameController => FrameController::properties(),
            Self::ViveFocus3 => ViveFocus3::properties(),
            Self::HolographicController => HolographicController::properties(),
            Self::HPMotionController => ReverbG2Controller::properties(),
            Self::ViveCosmosController => ViveCosmos::properties(),
            Self::PicoController => Pico4::properties(),
            // Nothing converts to or from the gamepad's bindings.
            Self::Gamepad | Self::GenericHmd | Self::Unknown(_) => return None,
        })
    }
}

/// Used to make sure every profile ends up in our profile list.
/// Do not manually implement. Or else...
#[diagnostic::on_unimplemented(
    message = "{Self} must be added to the profile list in `input::profiles::run_for_all_profiles`"
)]
pub trait SupportedProfile {}

// Vive tracker profile is like a fake profile, and when we're doing something in the context
// of all profiles, like suggesting bindings, we typically don't want to do it with the
// tracker profile.
#[cfg(feature = "monado")]
impl SupportedProfile for vive_tracker::ViveTracker {}

// The gamepad isn't a hand controller: the profiles below are probed for the hands, get legacy
// bindings and stand in for each other's bindings, none of which applies to it. Its bindings are
// only loaded from the game's gamepad bindings, see ControllerType::run_for_profile.
impl SupportedProfile for XboxController {}

pub fn run_for_all_profiles(runner: &mut impl RunWithProfile) {
    macro_rules! profile {
        ($profile:path) => {{
            #[allow(non_local_definitions)]
            impl SupportedProfile for $profile {}
            runner.run::<$profile>();
            if !runner.keep_running() {
                return;
            }
        }};
        () => {};
    }

    profile!(ViveWands);
    profile!(FrameController);
    profile!(Knuckles);
    profile!(vrlink_hand::VRLinkHand);
    profile!(MetaTouchPlus);
    profile!(TouchPro);
    profile!(OculusTouch);
    profile!(ViveFocus3);
    profile!(ViveCosmos);
    profile!(Pico4);
    profile!(PicoNeo3);
    profile!(SimpleController);
    profile!(wmr::hp_motion_controller::ReverbG2Controller);
    profile!(wmr::ms_motion_controller::HolographicController);
    profile!(wmr::samsung_odyssey_controller::SamsungOdysseyController);
}

pub struct InputToXrPath<'a, P: InteractionProfile> {
    instance: &'a xr::Instance,
    _marker: std::marker::PhantomData<P>,
}

#[allow(private_bounds)]
impl<'a, P: InteractionProfile> InputToXrPath<'a, P> {
    pub fn new(instance: &'a xr::Instance) -> InputToXrPath<'a, P> {
        Self {
            instance,
            _marker: Default::default(),
        }
    }
    pub fn into<T, M>(&self) -> Vec<xr::Path>
    where
        T: InputPath,
        P::LegalPaths: ContainsPath<T, M>,
    {
        vec![self.instance.string_to_path(&T::DYN.to_string()).unwrap()]
    }

    pub fn leftright<Subpath, Component, M1, M2>(&self) -> Vec<xr::Path>
    where
        Left<Subpath, Component>: InputPath,
        Right<Subpath, Component>: InputPath,
        P::LegalPaths: ContainsPath<Left<Subpath, Component>, M1>,
        P::LegalPaths: ContainsPath<Right<Subpath, Component>, M2>,
    {
        vec![
            self.instance
                .string_to_path(&Left::<Subpath, Component>::DYN.to_string())
                .unwrap(),
            self.instance
                .string_to_path(&Right::<Subpath, Component>::DYN.to_string())
                .unwrap(),
        ]
    }

    pub fn haptics(&self) -> Vec<xr::Path> {
        vec![
            self.instance
                .string_to_path("/user/hand/left/output/haptic")
                .unwrap(),
            self.instance
                .string_to_path("/user/hand/right/output/haptic")
                .unwrap(),
        ]
    }

    /// Both rumble motors of the gamepad.
    pub fn gamepad_haptics(&self) -> Vec<xr::Path> {
        GAMEPAD_HAPTICS
            .iter()
            .map(|path| self.instance.string_to_path(path).unwrap())
            .collect()
    }

    pub fn pose(&self) -> Vec<xr::Path> {
        vec![
            self.instance
                .string_to_path("/user/hand/left/input/grip/pose")
                .unwrap(),
            self.instance
                .string_to_path("/user/hand/right/input/grip/pose")
                .unwrap(),
        ]
    }
}

/// The gamepad's rumble motors, which its one SteamVR haptic output (`/output/haptic`) drives.
pub const GAMEPAD_HAPTICS: [&str; 2] = [
    "/user/gamepad/output/haptic_left",
    "/user/gamepad/output/haptic_right",
];

pub trait LegalPathsT {
    fn is_legal(path: DynInputPath) -> bool;
}

impl LegalPathsT for () {
    fn is_legal(_: DynInputPath) -> bool {
        false
    }
}

impl<T: InputPath> LegalPathsT for (T,) {
    #[inline]
    fn is_legal(path: DynInputPath) -> bool {
        path == T::DYN
    }
}
impl<T: InputPath, Tail> LegalPathsT for (T, Tail)
where
    Tail: LegalPathsT,
{
    #[inline]
    fn is_legal(path: DynInputPath) -> bool {
        path == T::DYN || Tail::is_legal(path)
    }
}

macro_rules! legal_paths {
    (Gamepad::<($first_s:ty, $first_c:ty) $(, ($rest_s:ty, $rest_c:ty) )*$(,)?>) => {
        $crate::input::profiles::__recursify_tuple![
            $crate::input::profiles::Gamepad<$first_s, $first_c>,
            $(
                $crate::input::profiles::Gamepad<$rest_s, $rest_c>,
            )*
        ]
    };
    (
        $(Both::<($both_first_s:ty, $both_first_c:ty) $(, ($both_rest_s:ty, $both_rest_c:ty) )*$(,)?>)?
        $(,Left::<($left_first_s:ty, $left_first_c:ty) $(, ($left_rest_s:ty, $left_rest_c:ty) )*$(,)?>)?
        $(,Right::<($right_first_s:ty, $right_first_c:ty) $(, ($right_rest_s:ty, $right_rest_c:ty) )*$(,)?>)?
    ) => {
        $crate::input::profiles::__recursify_tuple![
            $(
                $crate::input::profiles::Left<$both_first_s, $both_first_c>,
                $crate::input::profiles::Right<$both_first_s, $both_first_c>,
                $(
                    $crate::input::profiles::Left<$both_rest_s, $both_rest_c>,
                    $crate::input::profiles::Right<$both_rest_s, $both_rest_c>,
                )*
            )?
            $(
                $crate::input::profiles::Left<$left_first_s, $left_first_c>,
                $(
                    $crate::input::profiles::Left<$left_rest_s, $left_rest_c>,
                )*
            )?
            $(
                $crate::input::profiles::Right<$right_first_s, $right_first_c>,
                $(
                    $crate::input::profiles::Right<$right_rest_s, $right_rest_c>,
                )*
            )?
        ]
    }
}
use legal_paths;

/// Transforms a tuple (A, B, C) into (A, (B, (C,)))
macro_rules! __recursify_tuple {
    ($current:ty$(,)?) => { ($current,) };
    ($current:ty $(,$rest:ty)*$(,)? ) => {
        ($current, $crate::input::profiles::__recursify_tuple!($($rest),*))
    };
}
use __recursify_tuple;

mod typemagic {
    use super::*;
    // Some type magic for determining if a recursive tuple contains a particular input path.
    // The ideas here are essentially the same as the `HList` type from the `frunk` crate.

    #[diagnostic::on_unimplemented(message = "{T} is not a legal path")]
    pub trait ContainsPath<T, M> {}

    /// A marker for ContainsPath, so we can avoid overlapping blanket implementations.
    trait Marker {}
    /// The type is directly visible
    pub struct Here;
    /// The type is inside of T
    pub struct InsideOf<T>(PhantomData<T>);

    impl Marker for Here {}
    impl<M: Marker> Marker for InsideOf<M> {}

    // Single item tuple: this contains our path
    impl<T: InputPath> ContainsPath<T, Here> for (T,) {}
    impl<T: InputPath, Tail> ContainsPath<T, Here> for (T, Tail) {}
    // Here, `Tail` is a recursive tuple, like (A, (B, (C,)))
    impl<T: InputPath, M: Marker, Head, Tail> ContainsPath<T, InsideOf<M>> for (Head, Tail) where
        Tail: ContainsPath<T, M>
    {
    }
}

pub enum Property<T> {
    BothHands(T),
    PerHand { left: T, right: T },
}

impl<T> Property<T> {
    pub fn get(&self, hand: Hand) -> &T {
        match self {
            Self::BothHands(property) => property,
            Self::PerHand { left, right } => match hand {
                Hand::Left => left,
                Hand::Right => right,
            },
        }
    }
}

pub struct ProfileProperties {
    /// Corresponds to Prop_ModelNumber_String
    /// Can be pulled from a SteamVR System Report
    pub model: Property<&'static CStr>,
    /// Corresponds to Prop_ControllerType_String
    /// Can be pulled from a SteamVR System Report
    pub openvr_controller_type: &'static CStr,
    /// Corresponds to RenderModelName_String
    /// Can be found in SteamVR under resources/rendermodels (some are in driver subdirs)
    pub render_model_name: Property<&'static CStr>,
    pub main_axis: MainAxisType,
    /// Corresponds to Prop_RegisteredDeviceType_String
    pub registered_device_type: Property<&'static CStr>,
    /// Corresponds to Prop_SerialNumber_String
    pub serial_number: Property<&'static CStr>,
    /// Corresponds to Prop_TrackingSystemName_String
    pub tracking_system_name: &'static CStr,
    /// Corresponds to Prop_ManufacturerName_String
    pub manufacturer_name: &'static CStr,
    /// Corresponds to Prop_InputProfilePath_String
    /// Path to the input profile JSON, e.g. "{oculus}/input/touch_profile.json"
    pub input_profile_path: &'static CStr,
    /// Corresponds to Prop_SupportedButtons_Uint64
    /// Can be pulled from a SteamVR System Report
    pub legacy_buttons_mask: u64,
    /// Properties reported for the HMD while this profile is active.
    pub hmd: HmdProperties,
}

/// HMD identity properties. Manufacturer and tracking system name are shared with the
/// controllers (see [ProfileProperties]).
pub struct HmdProperties {
    /// Corresponds to Prop_ModelNumber_String
    /// Can be pulled from a SteamVR System Report
    pub model: &'static CStr,
    /// Corresponds to Prop_ControllerType_String
    /// Note that this is *not* the controllers' type - SteamVR reports e.g. "rift" or "indexhmd" here.
    pub controller_type: &'static CStr,
    /// Corresponds to Prop_SerialNumber_String
    pub serial_number: &'static CStr,
}

pub enum MainAxisType {
    Thumbstick,
    Trackpad,
}

#[derive(Debug)]
pub struct PoseTransformations {
    pub left_hand: Mat4,
    pub right_hand: Mat4,
}

// Some strong typing for representing input paths.

pub struct Left<Sub = (), Comp = ()>(PhantomData<(Sub, Comp)>);
pub struct Right<Sub = (), Comp = ()>(PhantomData<(Sub, Comp)>);
pub struct Gamepad<Sub = (), Comp = ()>(PhantomData<(Sub, Comp)>);

/// Represents the input subpath of a path, i.e. the `trigger` part of
/// /user/hand/left/input/trigger/click
trait Subpath {
    const DYN: paths::DynSubpath;
}

/// Represents the final component of an input path, i.e. the `click` part
/// of /user/hand/left/input/trigger/click
pub trait Component {
    type Output: xr::ActionInput;
    const DYN: Option<paths::DynComponent>;
}

// Vec2 paths can omit the final component
impl Component for () {
    type Output = xr::Vector2f;
    const DYN: Option<paths::DynComponent> = None;
}

/// Marker trait to signal a legal end component for a given subpath
#[diagnostic::on_unimplemented(message = "{Self} is not a legal component for {P}")]
trait LegalComponentFor<P: Subpath>: Component {}

/// Represents a complete input path: device, subpath, and component
trait InputPath {
    const DYN: DynInputPath;
}

impl<S, C> InputPath for Left<S, C>
where
    S: Subpath,
    C: LegalComponentFor<S>,
{
    const DYN: DynInputPath = DynInputPath {
        device: InputDevice::Hand(Hand::Left),
        subpath: S::DYN,
        component: C::DYN,
    };
}

impl<S, C> InputPath for Right<S, C>
where
    S: Subpath,
    C: LegalComponentFor<S>,
{
    const DYN: DynInputPath = DynInputPath {
        device: InputDevice::Hand(Hand::Right),
        subpath: S::DYN,
        component: C::DYN,
    };
}

impl<S, C> InputPath for Gamepad<S, C>
where
    S: Subpath,
    C: LegalComponentFor<S>,
{
    const DYN: DynInputPath = DynInputPath {
        device: InputDevice::Gamepad,
        subpath: S::DYN,
        component: C::DYN,
    };
}

/// The device an input is on, named by its top level path.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum InputDevice {
    Hand(Hand),
    /// `/user/gamepad`
    Gamepad,
}

impl InputDevice {
    /// The hands, left first, then the gamepad: the order of [devices::SubactionPaths::all],
    /// which priority masks and analog action states index devices by.
    ///
    /// [devices::SubactionPaths::all]: super::devices::SubactionPaths::all
    pub const ALL: [Self; 3] = [
        Self::Hand(Hand::Left),
        Self::Hand(Hand::Right),
        Self::Gamepad,
    ];

    pub fn path(self) -> &'static str {
        match self {
            Self::Hand(Hand::Left) => "/user/hand/left",
            Self::Hand(Hand::Right) => "/user/hand/right",
            Self::Gamepad => "/user/gamepad",
        }
    }
}

/// A dynamic representation of an input path, typically parsed from a string.
/// i.e.: /user/hand/left/input/trigger/click
/// This is not guaranteed to be an actually valid path!
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct DynInputPath {
    pub device: InputDevice,
    pub subpath: paths::DynSubpath,
    pub component: Option<paths::DynComponent>,
}

impl DynInputPath {
    pub fn with_component(self, component: paths::DynComponent) -> Self {
        Self {
            component: Some(component),
            ..self
        }
    }
}

impl std::fmt::Display for DynInputPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/input/{}", self.device.path(), self.subpath)?;
        if let Some(component) = &self.component {
            write!(f, "/{component}")?;
        }
        Ok(())
    }
}
impl std::str::FromStr for DynInputPath {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut split = s.split('/');
        macro_rules! verify_next {
            ($string:literal) => {
                match split
                    .next()
                    .ok_or_else(|| format!(concat!("missing component ", $string)))?
                {
                    $string => {}
                    other => return Err(format!("expected component {}, got {other}", $string)),
                }
            };
        }

        verify_next!("");
        verify_next!("user");

        let device = match split
            .next()
            .ok_or_else(|| "missing device component".to_string())?
        {
            "gamepad" => InputDevice::Gamepad,
            "hand" => match split
                .next()
                .ok_or_else(|| "missing hand component".to_string())?
            {
                "left" => InputDevice::Hand(Hand::Left),
                "right" => InputDevice::Hand(Hand::Right),
                other => return Err(format!("expected left or right hand, got {other}")),
            },
            other => return Err(format!("expected hand or gamepad, got {other}")),
        };

        verify_next!("input");

        let subpath_str = split.next();
        let subpath = match subpath_str {
            // SteamVR's names for the gamepad's view, menu and Xbox buttons
            // (drivers/gamepad/resources/input/gamepad_profile.json).
            Some("back") if device == InputDevice::Gamepad => paths::DynSubpath::View,
            Some("start") if device == InputDevice::Gamepad => paths::DynSubpath::Menu,
            Some("guide") if device == InputDevice::Gamepad => paths::DynSubpath::System,
            // Index finger curls have no OpenXR input. They follow the trigger and grip values,
            // which finger curl is estimated from anyway.
            Some("finger") => match split.next() {
                Some("index") => paths::DynSubpath::Trigger,
                Some("middle" | "ring" | "pinky") => paths::DynSubpath::Squeeze,
                other => return Err(format!("invalid finger {other:?}")),
            },
            // The Index's pinch gesture, which SteamVR derives from the trigger value, the index
            // finger's touch and the trackpad force. The trigger is its value.
            Some("pinch") => paths::DynSubpath::Trigger,
            _ => subpath_str
                .and_then(paths::DynSubpath::from_openvr_str)
                .ok_or_else(|| format!("invalid subpath {subpath_str:?}"))?,
        };

        let component = split.next();
        let component = component
            .map(|c| {
                paths::DynComponent::from_openvr_str(c).ok_or_else(|| {
                    format!("invalid component {component:?} for subpath {subpath_str:?}")
                })
            })
            .transpose()?;

        Ok(DynInputPath {
            device,
            subpath,
            component,
        })
    }
}

pub mod paths {
    use std::fmt::Display;

    use super::*;
    macro_rules! components {
        ($enum:ident, $($name:ident::<$output_type:ty>),+) => {
            $(
                pub struct $name;
                impl Component for $name {
                    type Output = $output_type;
                    const DYN: Option<$enum> = Some($enum::$name);
                }
            )+

            /// Represents the final component of an input path.
            /// The "click" in "/user/hand/input/a/click"
            #[derive(Copy, Clone, PartialEq, Eq)]
            pub enum $enum {
                $($name,)+
            }
        };
    }

    components!(
        DynComponent,
        Click::<bool>,
        Touch::<bool>,
        Value::<f32>,
        Force::<f32>,
        Vec2X::<f32>,
        Vec2Y::<f32>
    );

    impl DynComponent {
        pub fn from_openvr_str(s: &str) -> Option<Self> {
            match s {
                "click" => Some(Self::Click),
                "touch" => Some(Self::Touch),
                "value" | "pull" => Some(Self::Value),
                _ => None,
            }
        }
    }

    impl Display for DynComponent {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let s = match self {
                Self::Click => "click",
                Self::Touch => "touch",
                Self::Value => "value",
                Self::Force => "force",
                Self::Vec2X => "x",
                Self::Vec2Y => "y",
            };

            f.write_str(s)
        }
    }

    /// The path segment of a subpath: its snake_case name unless given explicitly.
    macro_rules! subpath_str {
        ($name:ident) => {
            paste::paste!(stringify!([<$name:snake>]))
        };
        ($name:ident, $path:literal) => {
            $path
        };
    }

    macro_rules! subpaths {
        ($enum:ident, $($name:ident $(($path:literal))?::<$component1:ident $(, $component_rest:ident)*>),+) => {
            $(
                pub struct $name;
                impl Subpath for $name {
                    const DYN: $enum = $enum::$name;
                }
                impl LegalComponentFor<$name> for $component1 {}
                $(
                    impl LegalComponentFor<$name> for $component_rest {}
                )*
            )+

            /// Represents a specific input on an input path
            /// The "a" in "/user/hand/input/a/click"
            #[derive(Copy, Clone, PartialEq, Eq, Debug)]
            pub enum $enum {
                $($name,)+
            }

            impl Display for $enum {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    let s = match self {
                        $(Self::$name => subpath_str!($name $(, $path)?),)+
                    };
                    f.write_str(s)
                }
            }
        }
    }

    subpaths!(
        DynSubpath,
        A::<Click, Touch>,
        B::<Click, Touch>,
        X::<Click, Touch>,
        Y::<Click, Touch>,
        Menu::<Click, Touch>,
        View::<Click, Touch>,
        System::<Click, Touch>,
        Bumper::<Click, Touch>,
        // The Cosmos bumpers, which SteamVR calls bumper.
        Shoulder::<Click>,
        DpadUp::<Click, Touch>,
        DpadLeft::<Click, Touch>,
        DpadDown::<Click, Touch>,
        DpadRight::<Click, Touch>,
        Select::<Click>,
        Trigger::<Click, Value, Force, Touch>,
        Squeeze::<Click, Value, Force, Touch>,
        Thumbstick::<Click, Touch, Vec2X, Vec2Y>,
        Trackpad::<Click, Touch, Force, Vec2X, Vec2Y>,
        Thumbrest::<Touch>,
        PinchExt("pinch_ext")::<Value>,
        GraspExt("grasp_ext")::<Value>,
        // The gamepad's.
        ShoulderLeft::<Click>,
        ShoulderRight::<Click>,
        TriggerLeft::<Value>,
        TriggerRight::<Value>,
        ThumbstickLeft::<Click, Vec2X, Vec2Y>,
        ThumbstickRight::<Click, Vec2X, Vec2Y>
    );

    // Vec2 impls
    impl LegalComponentFor<Thumbstick> for () {}
    impl LegalComponentFor<Trackpad> for () {}
    impl LegalComponentFor<ThumbstickLeft> for () {}
    impl LegalComponentFor<ThumbstickRight> for () {}

    impl DynSubpath {
        pub fn from_openvr_str(s: &str) -> Option<Self> {
            match s {
                "a" => Some(Self::A),
                "b" => Some(Self::B),
                "x" => Some(Self::X),
                "y" => Some(Self::Y),
                "dpad_up" => Some(Self::DpadUp),
                "dpad_left" => Some(Self::DpadLeft),
                "dpad_down" => Some(Self::DpadDown),
                "dpad_right" => Some(Self::DpadRight),
                "application_menu" | "menu" => Some(Self::Menu),
                "view" => Some(Self::View),
                "system" => Some(Self::System),
                "bumper" => Some(Self::Bumper),
                "trigger" => Some(Self::Trigger),
                "grip" => Some(Self::Squeeze),
                "thumbstick" | "joystick" => Some(Self::Thumbstick),
                "thumbrest" => Some(Self::Thumbrest),
                "trackpad" => Some(Self::Trackpad),
                "shoulder_left" => Some(Self::ShoulderLeft),
                "shoulder_right" => Some(Self::ShoulderRight),
                "trigger_left" => Some(Self::TriggerLeft),
                "trigger_right" => Some(Self::TriggerRight),
                "joystick_left" | "thumbstick_left" => Some(Self::ThumbstickLeft),
                "joystick_right" | "thumbstick_right" => Some(Self::ThumbstickRight),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DynInputPath, InputDevice};
    use crate::openxr_data::Hand;

    #[test]
    fn gamepad_paths() {
        let parse = |path: &str| path.parse::<DynInputPath>();
        let path = |path: &str| parse(path).unwrap().to_string();
        assert!(parse("/user/gamepad/input/a/click").unwrap().device == InputDevice::Gamepad);
        assert_eq!(
            path("/user/gamepad/input/joystick_left/click"),
            "/user/gamepad/input/thumbstick_left/click"
        );
        assert_eq!(
            path("/user/gamepad/input/trigger_right/pull"),
            "/user/gamepad/input/trigger_right/value"
        );
        // SteamVR's names for the view and menu buttons, only on the gamepad.
        assert_eq!(path("/user/gamepad/input/back"), "/user/gamepad/input/view");
        assert_eq!(
            path("/user/gamepad/input/start/click"),
            "/user/gamepad/input/menu/click"
        );
        assert!(parse("/user/hand/left/input/back").is_err());
        assert!(
            parse("/user/hand/right/input/a/click").unwrap().device
                == InputDevice::Hand(Hand::Right)
        );
        assert!(parse("/user/head/input/a/click").is_err());
    }
}
