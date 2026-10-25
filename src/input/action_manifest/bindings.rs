#![allow(private_interfaces)]

use super::context::BindingsProfileLoadContext;
use crate::input::action_manifest::context;
use crate::input::custom_bindings::BoolBindingData;
use crate::input::haptics::{BindingHaptic, BindingPress, Vibration};
use crate::input::profiles::paths::DynComponent;
use crate::input::profiles::{Component, DynInputPath, paths};
use crate::input::{ActionData, BoundPoseType};
use crate::{
    input::custom_bindings::{
        self, BoolBindingType, ChordData, DoubleTapData, DpadData, DpadDirection, GrabBindingData,
        LatchData, ThresholdBindingData, TimedPress, TimedPressData, ToggleData, Vector2Inversion,
        Vector2Transform,
    },
    openxr_data::Hand,
};
use log::{debug, trace, warn};
use openxr as xr;
use serde::de::value::StringDeserializer;
use serde::{
    Deserialize,
    de::{Error, IgnoredAny, Unexpected},
};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

/**
 * Structure for binding files
 */

#[derive(Clone, Default, Deserialize)]
pub struct Bindings {
    #[serde(deserialize_with = "lenient_sets")]
    pub bindings: HashMap<String, ActionSetBinding>,
    #[serde(default, deserialize_with = "binding_options")]
    pub options: BindingOptions,
}

/// The `options` of a bindings file that xrizer uses.
#[derive(Clone, Default)]
pub struct BindingOptions {
    /// `variant`, e.g. "singlecontroller" in Half-Life: Alyx's single controller bindings, which
    /// games read with GetBindingVariant.
    pub variant: Option<String>,
    /// `mirror_actions` of legacy bindings: both hands drive the actions of
    /// /actions/legacy_mirrored, rather than their own of /actions/legacy.
    pub mirror_actions: bool,
    /// `simulated_controller_type`: the controller type apps are shown instead of the real one.
    pub simulated_controller_type: Option<String>,
}

fn binding_options<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BindingOptions, D::Error> {
    let options = serde_json::Value::deserialize(d)?;
    let string = |name| {
        options[name]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok(BindingOptions {
        variant: string("variant"),
        // The binding UI saves a boolean; a string is accepted too.
        mirror_actions: options["mirror_actions"].as_bool().unwrap_or(false)
            || options["mirror_actions"].as_str() == Some("true"),
        simulated_controller_type: string("simulated_controller_type")
            .filter(|ty| !ty.eq_ignore_ascii_case("none")),
    })
}

// SteamVR skips what it doesn't understand, so one odd entry (an unknown mode, a missing
// field, a tracker pose path...) only drops that entry rather than the whole file.
#[derive(Clone, Deserialize)]
pub struct ActionSetBinding {
    #[serde(default, deserialize_with = "lenient_vec")]
    pub sources: Vec<ActionBinding>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub poses: Vec<PoseBinding>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub haptics: Vec<SimpleActionBinding>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub skeleton: Vec<SimpleActionBinding>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub chords: Vec<ChordBinding>,
}

/// An output that is active while all of its inputs are.
#[derive(Clone, Deserialize)]
pub struct ChordBinding {
    output: ActionPath,
    /// Input paths and the slot of each (click, touch, a dpad direction...)
    inputs: Vec<(MaybeInputPath, String)>,
}

/// Deserializes a list, skipping (and logging) the entries that don't parse.
pub fn lenient_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let entries = match serde_json::Value::deserialize(d)? {
        serde_json::Value::Array(entries) => entries,
        serde_json::Value::Null => Vec::new(),
        other => {
            warn!("Ignoring {other}, expected a list");
            Vec::new()
        }
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            T::deserialize(&entry)
                .inspect_err(|e| warn!("Ignoring invalid entry {entry}: {e}"))
                .ok()
        })
        .collect())
}

/// Deserializes a value, logging and replacing it with `None` if it doesn't parse.
fn lenient<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(d)?;
    Ok(Option::<T>::deserialize(&value)
        .inspect_err(|e| warn!("Ignoring invalid value {value}: {e}"))
        .ok()
        .flatten())
}

fn lenient_sets<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<HashMap<String, ActionSetBinding>, D::Error> {
    let sets = HashMap::<String, serde_json::Value>::deserialize(d)?;
    Ok(sets
        .into_iter()
        .filter_map(|(name, set)| {
            let set = ActionSetBinding::deserialize(&set)
                .inspect_err(|e| warn!("Ignoring bindings for action set {name}: {e}"))
                .ok()?;
            // Action set paths are case insensitive, like action paths.
            Some((name.to_lowercase(), set))
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct ActionPath {
    /// This is the full path as pulled from the manifest, but set to lowercase
    /// Action handles appear to be case insensitive.
    pub path: String,
}

impl ActionPath {
    /// Returns just the action name - the end part of the path - cleaned
    /// so that it's compatible with the OpenXR path semantics
    /// See Section 6.2 (Well-Formed Path Strings) of the OpenXR spec
    pub fn cleaned_name(&self) -> String {
        let name = self
            .path
            .rsplit_once('/')
            .map_or(&*self.path, |(_, name)| name);
        super::actions::xr_friendly(name)
    }

    /// The `/actions/<set>` part, if the path has one.
    pub fn action_set_name(&self) -> Option<&str> {
        let (set_end_idx, _) = self.path.match_indices('/').nth(2)?;
        Some(&self.path[0..set_end_idx])
    }
}

impl<'de> Deserialize<'de> for ActionPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Lowercased like the names given to GetActionHandle.
        String::deserialize(deserializer).map(|s| Self {
            path: s.to_lowercase(),
        })
    }
}

#[derive(Clone, Deserialize)]
pub struct PoseBinding {
    output: ActionPath,
    #[serde(deserialize_with = "parse_pose_binding")]
    path: (Hand, BoundPoseType),
}

fn parse_pose_binding<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<(Hand, BoundPoseType), D::Error> {
    let pose_path: &str = Deserialize::deserialize(d)?;

    let (hand, pose) = pose_path.rsplit_once('/').ok_or(D::Error::invalid_value(
        Unexpected::Str(pose_path),
        &"a value matching /user/hand/{left,right}/pose/<pose>",
    ))?;

    let hand = match hand {
        "/user/hand/left/pose" => Hand::Left,
        "/user/hand/right/pose" => Hand::Right,
        _ => {
            return Err(D::Error::unknown_variant(
                hand,
                &["/user/hand/left/pose", "/user/hand/right/pose"],
            ));
        }
    };

    let pose = match pose {
        "raw" => BoundPoseType::Raw,
        "tip" => BoundPoseType::Tip,
        "base" => BoundPoseType::Base,
        "gdc2015" => BoundPoseType::Gdc2015,
        "handgrip" => BoundPoseType::Handgrip,
        "grip" => BoundPoseType::Grip,
        "openxr_handmodel" => BoundPoseType::OpenxrHandmodel,
        "openxr_pinch" => BoundPoseType::OpenxrPinch,
        "openxr_poke" => BoundPoseType::OpenxrPoke,
        "openxr_aim" => BoundPoseType::OpenxrAim,
        "openxr_grip" => BoundPoseType::OpenxrGrip,
        other => {
            warn!("Unknown pose type: {other:?}");
            BoundPoseType::Raw
        }
    };

    Ok((hand, pose))
}

#[derive(Clone, Deserialize)]
pub struct SimpleActionBinding {
    output: ActionPath,
    path: String,
}

/// Note that when this is used, it's typically missing the final component
#[derive(Clone, Deserialize)]
#[serde(from = "String")]
pub enum MaybeInputPath {
    Valid(DynInputPath),
    Invalid { path: String, error: String },
}

impl From<String> for MaybeInputPath {
    fn from(value: String) -> Self {
        match value.parse() {
            Ok(path) => Self::Valid(path),
            Err(error) => Self::Invalid { path: value, error },
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionBinding {
    None(IgnoredAny),
    Button(ActionBindingData<ButtonInput, ButtonParameters>),
    ToggleButton(ActionBindingData<ButtonInput, ButtonParameters>),
    Dpad(ActionBindingData<DpadInput, DpadParameters>),
    /// Older name for a dpad with the click sub mode.
    DpadClick(ActionBindingData<DpadInput, DpadParameters>),
    /// Older name for a dpad with the touch sub mode.
    DpadTouch(ActionBindingData<DpadInput, DpadParameters>),
    Trigger(ActionBindingData<TriggerInput, ClickThresholdParams>),
    ScalarConstant(ActionBindingData<ScalarConstantInput, ScalarConstantParameters>),
    ForceSensor(ActionBindingData<ForceSensorInput, ForceSensorParameters>),
    Grab(ActionBindingData<GrabInput, GrabParameters>),
    Scroll(ActionBindingData<ScrollInput, ScrollParameters>),
    Trackpad(ActionBindingData<Vector2Input, Vector2Parameters>),
    Joystick(ActionBindingData<Vector2Input, Vector2Parameters>),
    /// A radial menu, as the binding UI names it. It offers no inputs for it, so its bindings
    /// have none.
    Radial(ActionBindingData<Option<RadialInput>, RadialParameters>),
    /// A radial menu, as SteamVR's runtime names it (see [handle_radial_binding]).
    RadialMenu(ActionBindingData<Option<RadialInput>, RadialParameters>),
}

impl ActionBinding {
    fn path_mut(&mut self) -> Option<&mut MaybeInputPath> {
        match self {
            Self::None(_) => None,
            Self::Radial(data) | Self::RadialMenu(data) => Some(&mut data.path),
            Self::Button(data) => Some(&mut data.path),
            Self::ToggleButton(data) => Some(&mut data.path),
            Self::Dpad(data) | Self::DpadClick(data) | Self::DpadTouch(data) => {
                Some(&mut data.path)
            }
            Self::Trigger(data) => Some(&mut data.path),
            Self::ScalarConstant(data) => Some(&mut data.path),
            Self::ForceSensor(data) => Some(&mut data.path),
            Self::Grab(data) => Some(&mut data.path),
            Self::Scroll(data) => Some(&mut data.path),
            Self::Trackpad(data) | Self::Joystick(data) => Some(&mut data.path),
        }
    }
}

impl Bindings {
    /// Every input path of the bindings, so they can be moved to another controller.
    pub fn input_paths_mut(&mut self) -> impl Iterator<Item = &mut MaybeInputPath> {
        self.bindings.values_mut().flat_map(|set| {
            set.sources
                .iter_mut()
                .filter_map(ActionBinding::path_mut)
                .chain(
                    set.chords
                        .iter_mut()
                        .flat_map(|chord| chord.inputs.iter_mut().map(|(path, _)| path)),
                )
        })
    }
}

#[derive(Clone, Deserialize)]
#[serde(bound(deserialize = "Inputs: Deserialize<'de>, Parameters: serde::de::DeserializeOwned"))]
struct ActionBindingData<Inputs, Parameters = ()> {
    path: MaybeInputPath,
    inputs: Inputs,
    /// Invalid parameters fall back to the defaults rather than dropping the binding.
    #[serde(default, deserialize_with = "lenient")]
    parameters: Option<Parameters>,
}

struct ValidActionBindingData<'a, Inputs, Parameters> {
    path: DynInputPath,
    inputs: &'a Inputs,
    parameters: Option<&'a Parameters>,
}

impl<Inputs, Parameters> ActionBindingData<Inputs, Parameters> {
    fn validate_path(
        &self,
        context: &BindingsProfileLoadContext,
    ) -> Option<ValidActionBindingData<'_, Inputs, Parameters>> {
        match &self.path {
            MaybeInputPath::Valid(path) => Some(ValidActionBindingData {
                path: *path,
                inputs: &self.inputs,
                parameters: self.parameters.as_ref(),
            }),
            MaybeInputPath::Invalid { path, error } => {
                context.unusable_binding(format_args!("invalid input path {path}: {error}"));
                None
            }
        }
    }
}

pub trait PathValidator: Fn(DynInputPath) -> Option<DynInputPath> {}
impl<F> PathValidator for F where F: Fn(DynInputPath) -> Option<DynInputPath> {}

#[derive(Deserialize, Debug)]
struct ActionBindingOutput<C> {
    output: ActionPath,
    #[serde(skip)]
    _marker: PhantomData<C>,
}

// Not derived, as that would require the marker to be Clone.
impl<C> Clone for ActionBindingOutput<C> {
    fn clone(&self) -> Self {
        Self {
            output: self.output.clone(),
            _marker: PhantomData,
        }
    }
}

/// Marker for actions that should be bound to a custom binding
#[derive(Debug)]
struct Custom;

struct InvalidActionPath<'a>(DynInputPath, &'a str);
impl InvalidActionPath<'_> {
    fn report(&self, context: &BindingsProfileLoadContext) {
        context.unusable_binding(format_args!("no {} for {}", self.0, self.1));
    }
}

impl<C: Component> ActionBindingOutput<C>
where
    C::Output: context::WithActionPattern,
{
    fn try_bind_with_component(
        &self,
        partial_input_path: DynInputPath,
        context: &mut BindingsProfileLoadContext<'_>,
        validator: impl PathValidator,
    ) -> Result<(), InvalidActionPath<'_>> {
        let complete_path = match C::DYN {
            Some(component) => partial_input_path.with_component(component),
            None => partial_input_path,
        };

        match validator(complete_path) {
            Some(path) => {
                context.try_suggest_binding::<C::Output>(self.output.path.clone(), path);
                Ok(())
            }
            None => Err(InvalidActionPath(complete_path, &self.output.path)),
        }
    }
}

#[repr(transparent)]
#[derive(Copy, Clone, derive_more::Deref)]
pub struct StringOrTyped<T>(T);

impl<T: FromStr> FromStr for StringOrTyped<T> {
    type Err = T::Err;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        T::from_str(s).map(Self)
    }
}

impl<T> From<T> for StringOrTyped<T> {
    fn from(t: T) -> Self {
        StringOrTyped(t)
    }
}

impl<'de, T: Deserialize<'de> + FromStr> Deserialize<'de> for StringOrTyped<T>
where
    T::Err: std::fmt::Display,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, Visitor};

        struct StringOrTypedVisitor<T>(PhantomData<T>);

        impl<'de, T> Visitor<'de> for StringOrTypedVisitor<T>
        where
            T: FromStr + Deserialize<'de>,
            T::Err: std::fmt::Display,
        {
            type Value = StringOrTyped<T>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(formatter, "a {} or string", std::any::type_name::<T>())
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                T::from_str(value)
                    .map(StringOrTyped)
                    .map_err(|_| E::custom("parse error"))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let value = T::deserialize(serde::de::value::I64Deserializer::<E>::new(value))
                    .map_err(E::custom)?;
                Ok(StringOrTyped(value))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let value = T::deserialize(serde::de::value::U64Deserializer::<E>::new(value))
                    .map_err(E::custom)?;
                Ok(StringOrTyped(value))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let value = T::deserialize(serde::de::value::F64Deserializer::<E>::new(value))
                    .map_err(E::custom)?;
                Ok(StringOrTyped(value))
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let value = T::deserialize(serde::de::value::BoolDeserializer::<E>::new(value))
                    .map_err(E::custom)?;
                Ok(StringOrTyped(value))
            }
        }

        deserializer.deserialize_any(StringOrTypedVisitor(PhantomData))
    }
}

#[derive(Clone, Deserialize)]
struct ButtonInput {
    touch: Option<ActionBindingOutput<paths::Touch>>,
    /// Click can be overridden to use a different path via the `force_input` parameter.
    click: Option<ActionBindingOutput<paths::Click>>,
    double: Option<ActionBindingOutput<Custom>>,
    /// Nonstandard slot some games use for grip bindings in button mode
    /// (e.g. Half-Life 2: VR); SteamVR treats it like click, so we do too.
    grab: Option<ActionBindingOutput<paths::Click>>,
    /// Active once the button has been held for the long press delay.
    long: Option<ActionBindingOutput<paths::Click>>,
    /// Like long.
    held: Option<ActionBindingOutput<paths::Click>>,
    /// A press that isn't part of a double tap or a long press.
    single: Option<ActionBindingOutput<paths::Click>>,
    #[serde(flatten)]
    unknown: HashMap<String, IgnoredAny>,
}

#[derive(Clone, Deserialize, Default)]
pub struct ClickThresholdParams {
    pub click_activate_threshold: Option<StringOrTyped<f32>>,
    pub click_deactivate_threshold: Option<StringOrTyped<f32>>,
    /// Played when an analog input crosses the thresholds.
    #[serde(flatten)]
    pub haptic: HapticParameters,
}

impl ClickThresholdParams {
    fn new_for_touch_conversion() -> Self {
        Self {
            click_activate_threshold: Some(0.01f32.into()),
            click_deactivate_threshold: Some(0.005f32.into()),
            ..Default::default()
        }
    }
}

/// The haptic SteamVR plays where a click crosses its threshold. An amplitude of 0 turns it off.
#[derive(Clone, Copy, Default, Deserialize)]
pub struct HapticParameters {
    haptic_amplitude: Option<StringOrTyped<f32>>,
    /// Hz
    haptic_frequency: Option<StringOrTyped<f32>>,
    /// Seconds, 0 for a single pulse
    haptic_duration: Option<StringOrTyped<f32>>,
    /// Seconds
    haptic_delay: Option<StringOrTyped<f32>>,
    /// The release's, the press's when unset.
    off_haptic_amplitude: Option<StringOrTyped<f32>>,
    off_haptic_frequency: Option<StringOrTyped<f32>>,
    off_haptic_duration: Option<StringOrTyped<f32>>,
}

impl HapticParameters {
    /// The haptic, with `default_amplitude` if the binding doesn't set one. None if it's off.
    fn haptic(&self, default_amplitude: f32) -> Option<BindingHaptic> {
        let get = |value: Option<StringOrTyped<f32>>| value.map(|v| *v).filter(|v| v.is_finite());
        let on = Vibration {
            duration: get(self.haptic_duration).unwrap_or(0.0),
            frequency: get(self.haptic_frequency).unwrap_or(BindingHaptic::DEFAULT_FREQUENCY),
            amplitude: get(self.haptic_amplitude).unwrap_or(default_amplitude),
        };
        let off = Vibration {
            duration: get(self.off_haptic_duration).unwrap_or(on.duration),
            frequency: get(self.off_haptic_frequency).unwrap_or(on.frequency),
            amplitude: get(self.off_haptic_amplitude).unwrap_or(on.amplitude),
        };
        let delay = get(self.haptic_delay)
            .and_then(|delay| Duration::try_from_secs_f32(delay).ok())
            .unwrap_or_default();
        BindingHaptic::new(on, off, delay)
    }
}

/// The value while pressed (on) and released (off), per axis.
#[derive(Clone, Deserialize)]
struct ScalarConstantParameters {
    #[serde(rename = "on/x")]
    on_x: Option<StringOrTyped<f32>>,
    #[serde(rename = "on/y")]
    on_y: Option<StringOrTyped<f32>>,
    #[serde(rename = "on/z")]
    on_z: Option<StringOrTyped<f32>>,
    #[serde(rename = "off/x")]
    off_x: Option<StringOrTyped<f32>>,
    #[serde(rename = "off/y")]
    off_y: Option<StringOrTyped<f32>>,
    #[serde(rename = "off/z")]
    off_z: Option<StringOrTyped<f32>>,
}

impl ScalarConstantParameters {
    /// The on and off values, with the binding UI's defaults (1 and 0) for missing or invalid ones.
    fn values(parameters: Option<&Self>) -> ([f32; 3], [f32; 3]) {
        let value = |get: fn(&Self) -> Option<StringOrTyped<f32>>, default: f32| {
            parameters
                .and_then(get)
                .map(|value| *value)
                .filter(|value| value.is_finite())
                .unwrap_or(default)
        };
        (
            [
                value(|p| p.on_x, 1.0),
                value(|p| p.on_y, 1.0),
                value(|p| p.on_z, 1.0),
            ],
            [
                value(|p| p.off_x, 0.0),
                value(|p| p.off_y, 0.0),
                value(|p| p.off_z, 0.0),
            ],
        )
    }
}

#[derive(Clone, Deserialize)]
struct ButtonParameters {
    #[serde(default, deserialize_with = "ButtonForceInput::default_deserialize")]
    force_input: Option<ButtonForceInput>,
    /// Seconds
    long_press_delay: Option<StringOrTyped<f32>>,
    /// Keeps the click held while the input is touched.
    #[serde(default, deserialize_with = "lenient")]
    touchy_click: Option<StringOrTyped<bool>>,
    #[serde(flatten)]
    click_threshold: ClickThresholdParams,
}

impl ButtonParameters {
    /// None if missing or not a valid duration (negative, NaN or too long).
    fn long_press_delay(&self) -> Option<Duration> {
        Duration::try_from_secs_f32(*self.long_press_delay?).ok()
    }

    fn touchy_click(&self) -> bool {
        self.touchy_click.is_some_and(|touchy| *touchy)
    }
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
enum ButtonForceInput {
    Click,
    Value,
    Force,
    Position,
}

impl ButtonForceInput {
    fn default_deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Option<ButtonForceInput>, D::Error> {
        let s: Option<String> = Deserialize::deserialize(d)?;
        let Some(s) = s else {
            return Ok(None);
        };
        if s.is_empty() {
            Ok(Some(Self::Click))
        } else {
            Self::deserialize(StringDeserializer::new(s)).map(Some)
        }
    }
}

#[derive(Clone, Deserialize, Debug)]
struct DpadInput {
    east: Option<ActionBindingOutput<Custom>>,
    south: Option<ActionBindingOutput<Custom>>,
    north: Option<ActionBindingOutput<Custom>>,
    west: Option<ActionBindingOutput<Custom>>,
    center: Option<ActionBindingOutput<Custom>>,
}

impl DpadInput {
    /// The actions bound to the directions, for logs.
    fn outputs(&self) -> String {
        [
            ("north", &self.north),
            ("east", &self.east),
            ("south", &self.south),
            ("west", &self.west),
            ("center", &self.center),
        ]
        .into_iter()
        .filter_map(|(direction, output)| {
            Some(format!("{} ({direction})", output.as_ref()?.output.path))
        })
        .collect::<Vec<_>>()
        .join(", ")
    }
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct DpadParameters {
    pub sub_mode: DpadSubMode,
    pub deadzone_pct: StringOrTyped<f32>,
    pub overlap_pct: StringOrTyped<f32>,
    pub sticky: StringOrTyped<bool>,
}

impl Default for DpadParameters {
    fn default() -> Self {
        let zones = custom_bindings::DpadZones::default();
        Self {
            sub_mode: DpadSubMode::Touch,
            deadzone_pct: StringOrTyped(zones.deadzone * 100.0),
            overlap_pct: StringOrTyped(zones.overlap * 100.0),
            sticky: StringOrTyped(zones.sticky),
        }
    }
}

impl DpadParameters {
    fn zones(&self) -> custom_bindings::DpadZones {
        let defaults = custom_bindings::DpadZones::default();
        let fraction = |pct: f32, default: f32| {
            let fraction = pct / 100.0;
            if fraction.is_finite() {
                fraction.clamp(0.0, 1.0)
            } else {
                default
            }
        };
        custom_bindings::DpadZones {
            deadzone: fraction(*self.deadzone_pct, defaults.deadzone),
            overlap: fraction(*self.overlap_pct, defaults.overlap),
            sticky: *self.sticky,
        }
    }
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum DpadSubMode {
    Click,
    Touch,
}

/// The outputs of a radial menu: `button_<n>` for its buttons, and `center`.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
struct RadialInput(HashMap<String, ActionBindingOutput<Custom>>);

#[derive(Clone, Deserialize)]
struct RadialParameters {
    #[serde(default, deserialize_with = "lenient")]
    num_buttons: Option<StringOrTyped<u32>>,
    // Like a dpad's, which SteamVR's radial menu shares its code with.
    #[serde(default, deserialize_with = "lenient")]
    sub_mode: Option<DpadSubMode>,
    #[serde(default, deserialize_with = "lenient")]
    deadzone_pct: Option<StringOrTyped<f32>>,
    #[serde(default, deserialize_with = "lenient")]
    overlap_pct: Option<StringOrTyped<f32>>,
    #[serde(default, deserialize_with = "lenient")]
    sticky: Option<StringOrTyped<bool>>,
}

impl RadialParameters {
    /// The most buttons SteamVR's radial menu takes is unknown. Steam Input's radial menus,
    /// which it seems to come from, take 20.
    const MAX_BUTTONS: u32 = 20;

    fn zones(&self) -> custom_bindings::DpadZones {
        let fraction = |pct: Option<StringOrTyped<f32>>, default: f32| {
            pct.map(|pct| *pct / 100.0)
                .filter(|f| f.is_finite())
                .map_or(default, |f| f.clamp(0.0, 1.0))
        };
        custom_bindings::DpadZones {
            deadzone: fraction(self.deadzone_pct, 0.25),
            // A menu picks one button.
            overlap: fraction(self.overlap_pct, 0.0),
            sticky: self.sticky.is_some_and(|sticky| *sticky),
        }
    }
}

#[derive(Clone, Deserialize)]
struct TriggerInput {
    pull: Option<ActionBindingOutput<paths::Value>>,
    touch: Option<ActionBindingOutput<paths::Touch>>,
    click: Option<ActionBindingOutput<paths::Click>>,
}

#[derive(Clone, Deserialize)]
struct ScalarConstantInput {
    value: ActionBindingOutput<paths::Value>,
}

#[derive(Clone, Deserialize)]
struct ForceSensorInput {
    force: ActionBindingOutput<paths::Force>,
}

#[derive(Clone, Deserialize)]
struct ForceSensorParameters {
    #[allow(unused)]
    haptic_amplitude: Option<String>,
}

#[derive(Clone, Deserialize)]
struct GrabInput {
    grab: ActionBindingOutput<Custom>,
}

#[derive(Clone, Deserialize)]
pub struct GrabParameters {
    pub value_hold_threshold: Option<StringOrTyped<f32>>,
    pub value_release_threshold: Option<StringOrTyped<f32>>,
    pub force_hold_threshold: Option<StringOrTyped<f32>>,
    pub force_release_threshold: Option<StringOrTyped<f32>>,
    /// m/s, see GrabBindingData
    pub downgrade_speed: Option<StringOrTyped<f32>>,
    pub drop_speed: Option<StringOrTyped<f32>>,
    // capsense_enable_threshold is ignored: SteamVR's binding UI only offers it on pinch sources
    // (the Index's /input/pinch), where the index finger's proximity enables the grab. OpenXR only
    // has the trigger's touch for that, and xrizer reads pinch as the trigger, whose pull needs
    // the finger on it anyway.
    #[serde(flatten)]
    pub haptic: HapticParameters,
}

#[derive(Clone, Deserialize)]
struct ScrollInput {
    scroll: ActionBindingOutput<()>,
}

#[derive(Clone, Deserialize)]
struct ScrollParameters {
    /// "discrete" (default) or "smooth"
    scroll_mode: Option<String>,
    smooth_scroll_multiplier: Option<StringOrTyped<f32>>,
}

#[derive(Clone, Deserialize)]
struct Vector2Input {
    position: Option<ActionBindingOutput<()>>,
    click: Option<ActionBindingOutput<paths::Click>>,
    touch: Option<ActionBindingOutput<paths::Touch>>,
}

#[derive(Clone, Deserialize)]
struct Vector2Parameters {
    #[serde(default)]
    invert: Vector2Inversion,
    deadzone_pct: Option<StringOrTyped<f32>>,
    maxzone_pct: Option<StringOrTyped<f32>>,
    /// Keeps the click held while the stick is deflected.
    #[serde(default, deserialize_with = "lenient")]
    sticky_click: Option<StringOrTyped<bool>>,
    /// How far the stick must be deflected to keep the click, and released to let it go.
    sticky_activate_threshold: Option<StringOrTyped<f32>>,
    sticky_deactivate_threshold: Option<StringOrTyped<f32>>,
}

impl Vector2Parameters {
    fn transform(&self) -> Vector2Transform {
        let fraction = |pct: Option<StringOrTyped<f32>>, default: f32| {
            pct.map(|pct| *pct / 100.0)
                .filter(|f| f.is_finite())
                .map_or(default, |f| f.clamp(0.0, 1.0))
        };
        Vector2Transform::Position {
            invert: self.invert,
            deadzone: fraction(self.deadzone_pct, 0.0),
            maxzone: fraction(self.maxzone_pct, 1.0),
        }
    }

    /// The deflection thresholds that keep the click held, if sticky_click is on. The defaults
    /// are the binding UI's.
    fn sticky_thresholds(&self) -> Option<ClickThresholdParams> {
        self.sticky_click
            .is_some_and(|sticky| *sticky)
            .then(|| ClickThresholdParams {
                click_activate_threshold: Some(
                    self.sticky_activate_threshold.unwrap_or(0.4f32.into()),
                ),
                click_deactivate_threshold: Some(
                    self.sticky_deactivate_threshold.unwrap_or(0.3f32.into()),
                ),
                ..Default::default()
            })
    }
}

#[cfg(test)]
#[test]
fn vector2_inversion_parse() {
    for (json, expected) in [
        (r#"{}"#, Vector2Inversion::None),
        (r#"{"invert": ""}"#, Vector2Inversion::None),
        (r#"{"invert": "x"}"#, Vector2Inversion::X),
        (r#"{"invert": "y"}"#, Vector2Inversion::Y),
        (r#"{"invert": "xy"}"#, Vector2Inversion::XY),
        (r#"{"invert": "unknown"}"#, Vector2Inversion::None),
    ] {
        let parameters: Vector2Parameters = serde_json::from_str(json).unwrap();
        assert_eq!(parameters.invert, expected, "{json}");
    }
}

#[cfg(test)]
#[test]
fn long_press_delay_parse() {
    for (json, expected) in [
        (r#"{}"#, None),
        (
            r#"{"long_press_delay": "0.25"}"#,
            Some(Duration::from_millis(250)),
        ),
        (r#"{"long_press_delay": 1}"#, Some(Duration::from_secs(1))),
        (r#"{"long_press_delay": "-1"}"#, None),
        (r#"{"long_press_delay": "NaN"}"#, None),
        (r#"{"long_press_delay": "1e20"}"#, None),
        (r#"{"long_press_delay": 1e20}"#, None),
    ] {
        let parameters: ButtonParameters = serde_json::from_str(json).unwrap();
        assert_eq!(parameters.long_press_delay(), expected, "{json}");
    }
}

#[cfg(test)]
#[test]
fn touchy_click_parse() {
    for (json, expected) in [
        (r#"{}"#, false),
        (r#"{"touchy_click": "true"}"#, true),
        (r#"{"touchy_click": true}"#, true),
        (r#"{"touchy_click": "false"}"#, false),
        (r#"{"touchy_click": "yes"}"#, false),
    ] {
        let parameters: ButtonParameters = serde_json::from_str(json).unwrap();
        assert_eq!(parameters.touchy_click(), expected, "{json}");
    }
    // An invalid value doesn't take the other parameters with it.
    let parameters: ButtonParameters =
        serde_json::from_str(r#"{"touchy_click": "yes", "force_input": "force"}"#).unwrap();
    assert!(matches!(
        parameters.force_input,
        Some(ButtonForceInput::Force)
    ));
}

#[cfg(test)]
#[test]
fn sticky_click_parse() {
    let thresholds = |json| {
        let parameters: Vector2Parameters = serde_json::from_str(json).unwrap();
        parameters.sticky_thresholds().map(|t| {
            (
                *t.click_activate_threshold.unwrap(),
                *t.click_deactivate_threshold.unwrap(),
            )
        })
    };
    assert_eq!(thresholds(r#"{}"#), None);
    assert_eq!(thresholds(r#"{"sticky_click": "false"}"#), None);
    assert_eq!(thresholds(r#"{"sticky_click": "true"}"#), Some((0.4, 0.3)));
    assert_eq!(
        thresholds(r#"{"sticky_click": true, "sticky_activate_threshold": "0.6"}"#),
        Some((0.6, 0.3))
    );
}

#[cfg(test)]
#[test]
fn scalar_constant_values_parse() {
    assert_eq!(ScalarConstantParameters::values(None), ([1.0; 3], [0.0; 3]));
    for (json, expected) in [
        (r#"{}"#, ([1.0; 3], [0.0; 3])),
        (
            r#"{"on/x": "-1", "off/y": 0.5, "on/z": 0}"#,
            ([-1.0, 1.0, 0.0], [0.0, 0.5, 0.0]),
        ),
        (r#"{"on/x": "NaN"}"#, ([1.0; 3], [0.0; 3])),
    ] {
        let parameters: ScalarConstantParameters = serde_json::from_str(json).unwrap();
        assert_eq!(
            ScalarConstantParameters::values(Some(&parameters)),
            expected,
            "{json}"
        );
    }
}

#[cfg(test)]
#[test]
fn numeric_and_boolean_parameters() {
    // JSON numbers and booleans work as well as their string forms.
    let parameters: DpadParameters =
        serde_json::from_str(r#"{"sub_mode": "click", "deadzone_pct": 25, "sticky": true}"#)
            .unwrap();
    assert_eq!(*parameters.deadzone_pct, 25.0);
    assert!(*parameters.sticky);

    let parameters: Vector2Parameters =
        serde_json::from_str(r#"{"deadzone_pct": 10, "maxzone_pct": "90"}"#).unwrap();
    assert_eq!(*parameters.deadzone_pct.unwrap(), 10.0);
    assert_eq!(*parameters.maxzone_pct.unwrap(), 90.0);
}

#[cfg(test)]
#[test]
fn dpad_zone_parameters() {
    use custom_bindings::DpadZones;
    for (json, expected) in [
        // SteamVR's defaults
        (
            r#"{"sub_mode": "touch"}"#,
            DpadZones {
                deadzone: 0.25,
                overlap: 0.5,
                sticky: false,
            },
        ),
        (
            r#"{"deadzone_pct": "75", "overlap_pct": "0", "sticky": "true"}"#,
            DpadZones {
                deadzone: 0.75,
                overlap: 0.0,
                sticky: true,
            },
        ),
        (
            r#"{"deadzone_pct": 150, "overlap_pct": "NaN"}"#,
            DpadZones {
                deadzone: 1.0,
                ..Default::default()
            },
        ),
    ] {
        let parameters: DpadParameters = serde_json::from_str(json).unwrap();
        assert_eq!(parameters.zones(), expected, "{json}");
    }
}

#[cfg(test)]
#[test]
fn radial_parameters_parse() {
    use custom_bindings::DpadZones;
    let parameters: RadialParameters =
        serde_json::from_str(r#"{"num_buttons": "6", "overlap_pct": 50, "sticky": "true"}"#)
            .unwrap();
    assert_eq!(parameters.num_buttons.map(|n| *n), Some(6));
    assert_eq!(
        parameters.zones(),
        DpadZones {
            deadzone: 0.25,
            overlap: 0.5,
            sticky: true,
        }
    );
    // An invalid count doesn't take the other parameters with it.
    let parameters: RadialParameters =
        serde_json::from_str(r#"{"num_buttons": "six", "deadzone_pct": "50"}"#).unwrap();
    assert!(parameters.num_buttons.is_none());
    assert_eq!(parameters.zones().deadzone, 0.5);
}

pub fn handle_dpad_binding(
    validate_path: &dyn PathValidator,
    parent_path: DynInputPath,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    context: &mut BindingsProfileLoadContext,
    DpadInput {
        east,
        south,
        north,
        west,
        center,
    }: &DpadInput,
    parameters: Option<&DpadParameters>,
) {
    // Would love to use the dpad extension here, but it doesn't seem to
    // support touch trackpad dpads.
    use DpadDirection::*;

    let bound_actions: Vec<(&ActionPath, DpadDirection)> = [
        (north, North),
        (east, East),
        (south, South),
        (west, West),
        (center, Center),
    ]
    .into_iter()
    .filter_map(|(output, direction)| {
        let output = &output.as_ref()?.output;
        context
            .accepts_custom_binding(output)
            .then_some((output, direction))
    })
    .collect();

    if bound_actions.is_empty() {
        warn!("Dpad mode, but no actions ({parent_path} in {action_set_name})");
        return;
    }

    let Some(actions) = context.get_dpad_actions(
        validate_path,
        parent_path,
        action_set_name,
        action_set,
        parameters,
    ) else {
        return;
    };

    // The directions of one binding engage together.
    let zones = Arc::new(custom_bindings::DpadZoneState::new(
        parameters.map_or_else(custom_bindings::DpadZones::default, DpadParameters::zones),
    ));
    for (path, direction) in bound_actions {
        context.add_custom_binding(
            path,
            parent_path.hand,
            parent_path,
            BoolBindingType::Dpad(DpadData::new(actions.clone(), direction, zones.clone())),
        );
    }
}

/// A radial menu: a dpad with `num_buttons` zones. SteamVR's runtime has one (vrclient's
/// CInputFilter_RadialMenu, named radial_menu), whose processing its dpad shares. It reads the
/// input's x and y, requires num_buttons and caps it. The rest is guessed: the buttons are
/// `button_<n>` from 0, the first at the north and the others clockwise like a dpad's
/// directions, with the dpad's parameters and zones that don't overlap.
fn handle_radial_binding(
    validate_path: &dyn PathValidator,
    path: DynInputPath,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    context: &mut BindingsProfileLoadContext,
    inputs: Option<&RadialInput>,
    parameters: Option<&RadialParameters>,
) {
    // What the binding UI writes.
    let Some(RadialInput(outputs)) = inputs.filter(|inputs| !inputs.0.is_empty()) else {
        debug!("Radial menu without outputs on {path} in {action_set_name}");
        return;
    };
    let Some((parameters, buttons)) =
        parameters.and_then(|p| p.num_buttons.map(|buttons| (p, *buttons)))
    else {
        crate::log_unique!(
            log::Level::Warn,
            "Ignoring radial menu on {path} in {action_set_name}: num_buttons is missing"
        );
        return;
    };
    if !(1..=RadialParameters::MAX_BUTTONS).contains(&buttons) {
        crate::log_unique!(
            log::Level::Warn,
            "Ignoring radial menu on {path} in {action_set_name}: {buttons} buttons"
        );
        return;
    }

    let mut bound_actions = Vec::new();
    for (slot, output) in outputs {
        let direction = match slot.as_str() {
            "center" => DpadDirection::Center,
            button => match button.strip_prefix("button_").map(str::parse::<u32>) {
                Some(Ok(button)) if button < buttons => DpadDirection::Button(button as usize),
                _ => {
                    crate::log_unique!(
                        log::Level::Warn,
                        "Ignoring unknown radial menu input {slot} on {path}"
                    );
                    continue;
                }
            },
        };
        if context.accepts_custom_binding(&output.output) {
            bound_actions.push((&output.output, direction));
        }
    }
    if bound_actions.is_empty() {
        return;
    }

    let Some(parent_path) = validate_path(path) else {
        for (output, _) in &bound_actions {
            InvalidActionPath(path, &output.path).report(context);
        }
        return;
    };
    // Engaged by touch like a dpad, unless the binding says otherwise.
    let dpad_parameters = DpadParameters {
        sub_mode: parameters.sub_mode.unwrap_or(DpadSubMode::Touch),
        ..Default::default()
    };
    let Some(actions) = context.get_dpad_actions(
        validate_path,
        parent_path,
        action_set_name,
        action_set,
        Some(&dpad_parameters),
    ) else {
        return;
    };

    let zones = Arc::new(custom_bindings::DpadZoneState::radial(
        parameters.zones(),
        buttons as usize,
    ));
    for (output, direction) in bound_actions {
        trace!(
            "suggesting {parent_path} for {} (radial menu {direction:?})",
            output.path
        );
        context.add_custom_binding(
            output,
            parent_path.hand,
            parent_path,
            BoolBindingType::Dpad(DpadData::new(actions.clone(), direction, zones.clone())),
        );
    }
}

/// A threshold on an analog input (or a boolean one, read as 0 or 1), if `output` can have one.
fn threshold_binding(
    output: &ActionPath,
    input: DynInputPath,
    params: Option<&ClickThresholdParams>,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    context: &mut BindingsProfileLoadContext,
) -> Option<BoolBindingType> {
    if !context.accepts_custom_binding(output) {
        return None;
    }
    input_threshold(input, params, action_set_name, action_set, context)
}

/// A threshold on an analog input (or a boolean one, read as 0 or 1), whatever it's used for.
fn input_threshold(
    input: DynInputPath,
    params: Option<&ClickThresholdParams>,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    context: &mut BindingsProfileLoadContext,
) -> Option<BoolBindingType> {
    Some(if input.component.is_none() {
        let action = context.input_action(action_set_name, action_set, input)?;
        BoolBindingType::ThresholdVec2(ThresholdBindingData::new(action, params))
    } else {
        let action = context.input_action(action_set_name, action_set, input)?;
        BoolBindingType::ThresholdFloat(ThresholdBindingData::new(action, params))
    })
}

/// Binds an analog input (or a boolean one, read as 0 or 1) through a threshold.
fn bind_threshold(
    output: &ActionPath,
    input: DynInputPath,
    params: Option<&ClickThresholdParams>,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    context: &mut BindingsProfileLoadContext,
) {
    if let Some(ty) = threshold_binding(output, input, params, action_set_name, action_set, context)
    {
        trace!("suggesting {input} for {} (threshold)", output.path);
        context.add_custom_binding(output, input.hand, input, ty);
    }
}

/// The input a button mode press reads: the component picked by `force_input`, else the click.
/// The flag says whether it is (or stands in for) the click, which is bound as is.
fn button_press_input(
    path: DynInputPath,
    force_input: Option<ButtonForceInput>,
    validate_path: &dyn PathValidator,
) -> Option<(DynInputPath, bool)> {
    // Default to value for clicky components, because the click point
    // does not necessarily match SteamVR's click point.
    let target = force_input.unwrap_or(ButtonForceInput::Value);
    let complete_path = match target {
        ButtonForceInput::Click => path.with_component(DynComponent::Click),
        ButtonForceInput::Value => path.with_component(DynComponent::Value),
        ButtonForceInput::Force => path.with_component(DynComponent::Force),
        ButtonForceInput::Position => path, // No component = 2D binding
    };
    match validate_path(complete_path) {
        Some(input) if input.component != Some(DynComponent::Click) => Some((input, false)),
        _ => {
            if !matches!(target, ButtonForceInput::Click) {
                debug!("falling back to click component on {path} (target: {target:?})");
            }
            validate_path(path.with_component(DynComponent::Click)).map(|input| (input, true))
        }
    }
}

/// The input a touch reads, with the thresholds to apply to it. SteamVR reports any pull as a
/// touch on inputs without a touch sensor, so analog inputs (picked by the profile or as a
/// fallback) get a tiny threshold instead of the runtime's click point.
fn touch_input(
    path: DynInputPath,
    validate_path: &dyn PathValidator,
) -> Option<(DynInputPath, ClickThresholdParams)> {
    let touch_path = path.with_component(DynComponent::Touch);
    match validate_path(touch_path) {
        Some(
            analog @ DynInputPath {
                component: Some(DynComponent::Value | DynComponent::Force),
                ..
            },
        ) => Some((analog, ClickThresholdParams::new_for_touch_conversion())),
        Some(touch) => Some((touch, ClickThresholdParams::default())),
        None => {
            let pull = validate_path(path.with_component(DynComponent::Value))?;
            debug!("Falling back to pull for touch on {path}");
            Some((pull, ClickThresholdParams::new_for_touch_conversion()))
        }
    }
}

/// Binds a touch output.
fn bind_touch(
    touch: &ActionBindingOutput<paths::Touch>,
    path: DynInputPath,
    validate_path: &dyn PathValidator,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    context: &mut BindingsProfileLoadContext,
) {
    let Some((input, params)) = touch_input(path, validate_path) else {
        InvalidActionPath(path.with_component(DynComponent::Touch), &touch.output.path)
            .report(context);
        return;
    };
    if input.component == Some(DynComponent::Value) || input.component == Some(DynComponent::Force)
    {
        bind_threshold(
            &touch.output,
            input,
            Some(&params),
            action_set_name,
            action_set,
            context,
        );
    } else {
        context.try_suggest_binding::<bool>(touch.output.path.clone(), input);
    }
}

/// Binds a boolean input to a custom binding built from an action reading it.
fn bind_bool_input(
    output: &ActionPath,
    input: DynInputPath,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    context: &mut BindingsProfileLoadContext,
    make: impl FnOnce(xr::Action<bool>) -> BoolBindingType,
) {
    if !context.accepts_custom_binding(output) {
        return;
    }
    let Some(action) = context.input_action(action_set_name, action_set, input) else {
        return;
    };
    context.add_custom_binding(output, input.hand, input, make(action));
}

/// The touch that keeps a button mode click held (touchy_click), with its thresholds. It's the
/// touch xrizer reports for the input (see [touch_input]), so a click never outlasts the binding's
/// own touch. A touch that is the click itself adds nothing.
fn touchy_hold(
    path: DynInputPath,
    validate_path: &dyn PathValidator,
) -> Option<(DynInputPath, ClickThresholdParams)> {
    touch_input(path, validate_path)
        .filter(|(touch, _)| touch.component != Some(DynComponent::Click))
}

/// A button mode press of `press`, kept held while `hold` (see [touchy_hold]) is, if given.
fn button_press_binding(
    output: &ActionPath,
    press: DynInputPath,
    thresholds: Option<&ClickThresholdParams>,
    hold: Option<&(DynInputPath, ClickThresholdParams)>,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    context: &mut BindingsProfileLoadContext,
) -> Option<BoolBindingType> {
    let press = threshold_binding(
        output,
        press,
        thresholds,
        action_set_name,
        action_set,
        context,
    )?;
    let hold = hold.and_then(|(touch, params)| {
        input_threshold(*touch, Some(params), action_set_name, action_set, context)
    });
    Some(match hold {
        Some(hold) => BoolBindingType::Latch(LatchData::new(press, hold)),
        None => press,
    })
}

/// Binds a stick's click that stays held while the stick is deflected (sticky_click).
fn bind_sticky_click(
    click: &ActionBindingOutput<paths::Click>,
    path: DynInputPath,
    sticky: &ClickThresholdParams,
    validate_path: &dyn PathValidator,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    context: &mut BindingsProfileLoadContext,
) {
    // Like plain clicks of this mode, missing ones are skipped silently.
    let Some(click_path) = validate_path(path.with_component(DynComponent::Click)) else {
        return;
    };
    let Some(press) = threshold_binding(
        &click.output,
        click_path,
        None,
        action_set_name,
        action_set,
        context,
    ) else {
        return;
    };
    let hold = validate_path(path).and_then(|position| {
        input_threshold(position, Some(sticky), action_set_name, action_set, context)
    });
    let ty = match hold {
        Some(hold) => BoolBindingType::Latch(LatchData::new(press, hold)),
        None => press,
    };
    trace!(
        "suggesting {click_path} for {} (sticky click)",
        click.output.path
    );
    context.add_custom_binding(&click.output, path.hand, click_path, ty);
}

/// The OpenVR path of the headset's proximity sensor.
const HEAD_PROXIMITY: &str = "/user/head/proximity";

/// Handles the bindings for the headset, which has no OpenXR inputs. Only buttons on its proximity
/// sensor are supported, which are driven by user presence.
pub fn handle_hmd_bindings(context: &mut context::BindingsLoadContext, bindings: &Bindings) {
    for (action_set_name, set) in &bindings.bindings {
        for source in &set.sources {
            match source {
                ActionBinding::Button(ActionBindingData {
                    path: MaybeInputPath::Invalid { path, .. },
                    inputs:
                        ButtonInput {
                            click: Some(click), ..
                        },
                    ..
                }) if path.eq_ignore_ascii_case(HEAD_PROXIMITY) => {
                    context.add_head_proximity_binding(&click.output)
                }
                ActionBinding::None(_) => {}
                _ => debug!(
                    "Ignoring headset binding in {action_set_name}: only {HEAD_PROXIMITY} clicks are supported"
                ),
            }
        }
    }
}

pub fn handle_sources(
    validate_path: &dyn PathValidator,
    context: &mut BindingsProfileLoadContext,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    sources: &[ActionBinding],
) {
    for mode in sources {
        match mode {
            ActionBinding::None(_) => {}
            ActionBinding::Radial(data) | ActionBinding::RadialMenu(data) => {
                let Some(ValidActionBindingData {
                    path,
                    inputs,
                    parameters,
                }) = data.validate_path(context)
                else {
                    continue;
                };
                handle_radial_binding(
                    validate_path,
                    path,
                    action_set_name,
                    action_set,
                    context,
                    inputs.as_ref(),
                    parameters,
                );
            }
            ActionBinding::ToggleButton(data) => {
                let Some(ValidActionBindingData {
                    path,
                    inputs: ButtonInput { touch, click, .. },
                    parameters: _,
                }) = data.validate_path(context)
                else {
                    continue;
                };

                if let Some(touch) = touch {
                    bind_touch(
                        touch,
                        path,
                        validate_path,
                        action_set_name,
                        action_set,
                        context,
                    );
                }

                if let Some(click) = click {
                    let click_path = path.with_component(DynComponent::Click);
                    let Some(click_path) = validate_path(click_path) else {
                        InvalidActionPath(click_path, &click.output.path).report(context);
                        continue;
                    };
                    trace!("suggesting {click_path} for {} (toggle)", click.output.path);
                    bind_bool_input(
                        &click.output,
                        click_path,
                        action_set_name,
                        action_set,
                        context,
                        |action| BoolBindingType::Toggle(ToggleData::new(action)),
                    );
                }
            }
            ActionBinding::Button(data) => {
                let Some(ValidActionBindingData {
                    path,
                    inputs:
                        ButtonInput {
                            touch,
                            click,
                            double,
                            grab,
                            long,
                            held,
                            single,
                            unknown,
                        },
                    parameters,
                }) = data.validate_path(context)
                else {
                    continue;
                };
                for slot in unknown.keys() {
                    warn!("Ignoring unknown button input {slot} on {path}");
                }

                if let Some(touch) = touch {
                    bind_touch(
                        touch,
                        path,
                        validate_path,
                        action_set_name,
                        action_set,
                        context,
                    );
                }

                let click_path = path.with_component(DynComponent::Click);
                if let Some(double) = double
                    && let Ok(complete_path) = validate_path(click_path)
                        .ok_or_else(|| InvalidActionPath(click_path, &double.output.path))
                        .inspect_err(|e| e.report(context))
                {
                    bind_bool_input(
                        &double.output,
                        complete_path,
                        action_set_name,
                        action_set,
                        context,
                        |action| BoolBindingType::DoubleTap(DoubleTapData::new(action)),
                    );
                }

                let Some((press, is_click)) =
                    button_press_input(path, parameters.and_then(|p| p.force_input), validate_path)
                else {
                    for output in [click, grab, long, held, single].into_iter().flatten() {
                        InvalidActionPath(click_path, &output.output.path).report(context);
                    }
                    continue;
                };
                let thresholds = parameters.map(|b| &b.click_threshold);
                let touchy = parameters.filter(|p| p.touchy_click()).and_then(|_| {
                    let hold = touchy_hold(path, validate_path);
                    if hold.is_none() {
                        debug!("Ignoring touchy_click on {path}, which has no touch");
                    }
                    hold
                });

                // A single press only has to wait when it could turn into something else.
                let long_delay = long.as_ref().or(held.as_ref()).map(|_| {
                    parameters
                        .and_then(ButtonParameters::long_press_delay)
                        .unwrap_or(TimedPressData::DEFAULT_LONG_PRESS_DELAY)
                });
                let single_waits = double.is_some() || long_delay.is_some();
                let immediate = [
                    click.as_ref(),
                    grab.as_ref(),
                    single.as_ref().filter(|_| !single_waits),
                ];
                for click in immediate.into_iter().flatten() {
                    if touchy.is_some() {
                        if let Some(ty) = button_press_binding(
                            &click.output,
                            press,
                            thresholds,
                            touchy.as_ref(),
                            action_set_name,
                            action_set,
                            context,
                        ) {
                            trace!(
                                "suggesting {press} for {} (touchy click)",
                                click.output.path
                            );
                            context.add_custom_binding(&click.output, path.hand, press, ty);
                        }
                    } else if is_click {
                        // The runtime's click point is used for clicks.
                        context.try_suggest_binding::<bool>(click.output.path.clone(), press);
                    } else {
                        bind_threshold(
                            &click.output,
                            press,
                            thresholds,
                            action_set_name,
                            action_set,
                            context,
                        );
                    }
                }

                let timed = [
                    (long.as_ref(), long_delay.map(TimedPress::Long)),
                    (held.as_ref(), long_delay.map(TimedPress::Long)),
                    (
                        single.as_ref().filter(|_| single_waits),
                        Some(TimedPress::Single {
                            double: double.is_some(),
                            long_delay,
                        }),
                    ),
                ];
                for (output, kind) in timed {
                    let (Some(output), Some(kind)) = (output, kind) else {
                        continue;
                    };
                    let Some(press_binding) = button_press_binding(
                        &output.output,
                        press,
                        thresholds,
                        touchy.as_ref(),
                        action_set_name,
                        action_set,
                        context,
                    ) else {
                        continue;
                    };
                    context.add_custom_binding(
                        &output.output,
                        path.hand,
                        press,
                        BoolBindingType::Timed(TimedPressData::new(press_binding, kind)),
                    );
                }
            }
            ActionBinding::Dpad(data)
            | ActionBinding::DpadClick(data)
            | ActionBinding::DpadTouch(data) => {
                let Some(ValidActionBindingData {
                    path,
                    inputs,
                    parameters,
                }) = data.validate_path(context)
                else {
                    continue;
                };
                let sub_mode = match mode {
                    ActionBinding::DpadClick(_) => Some(DpadSubMode::Click),
                    ActionBinding::DpadTouch(_) => Some(DpadSubMode::Touch),
                    _ => None,
                };
                let legacy_parameters = sub_mode.map(|sub_mode| DpadParameters {
                    sub_mode,
                    ..parameters.map_or_else(Default::default, |p| DpadParameters {
                        sub_mode,
                        deadzone_pct: p.deadzone_pct,
                        overlap_pct: p.overlap_pct,
                        sticky: p.sticky,
                    })
                });
                let parameters = legacy_parameters.as_ref().or(parameters);

                let Some(parent_path) = validate_path(path) else {
                    InvalidActionPath(path, &inputs.outputs()).report(context);
                    continue;
                };
                handle_dpad_binding(
                    validate_path,
                    parent_path,
                    action_set_name,
                    action_set,
                    context,
                    inputs,
                    parameters,
                );
            }
            ActionBinding::Trigger(data) => {
                let Some(ValidActionBindingData {
                    path,
                    inputs: TriggerInput { pull, touch, click },
                    parameters,
                }) = data.validate_path(context)
                else {
                    continue;
                };

                if let Some(pull) = pull {
                    let _ = pull
                        .try_bind_with_component(path, context, validate_path)
                        .inspect_err(|e| e.report(context));
                }

                if let Some(click) = click {
                    let click_path = path.with_component(DynComponent::Click);
                    let has_thresholds = parameters.is_some_and(|p| {
                        p.click_activate_threshold.is_some()
                            || p.click_deactivate_threshold.is_some()
                    });
                    match validate_path(click_path) {
                        // Without a physical click, SteamVR clicks the analog value at the
                        // binding's thresholds. Physical clicks stay as they are.
                        Some(
                            analog @ DynInputPath {
                                component: Some(DynComponent::Value | DynComponent::Force),
                                ..
                            },
                        ) if has_thresholds => bind_threshold(
                            &click.output,
                            analog,
                            parameters,
                            action_set_name,
                            action_set,
                            context,
                        ),
                        Some(click_path) => context
                            .try_suggest_binding::<bool>(click.output.path.clone(), click_path),
                        None => InvalidActionPath(click_path, &click.output.path).report(context),
                    }
                }

                if let Some(touch) = touch {
                    bind_touch(
                        touch,
                        path,
                        validate_path,
                        action_set_name,
                        action_set,
                        context,
                    );
                }
            }
            ActionBinding::ScalarConstant(data) => {
                let Some(ValidActionBindingData {
                    path,
                    inputs: ScalarConstantInput { value },
                    parameters,
                }) = data.validate_path(context)
                else {
                    continue;
                };
                let (on, off) = ScalarConstantParameters::values(parameters);
                if context.is_custom_scalar_constant(&value.output, on, off) {
                    let Some((press, _)) = button_press_input(path, None, validate_path) else {
                        InvalidActionPath(path, &value.output.path).report(context);
                        continue;
                    };
                    if let Some(press_binding) =
                        input_threshold(press, None, action_set_name, action_set, context)
                    {
                        trace!(
                            "suggesting {press} for {} (scalar constant {on:?}, {off:?})",
                            value.output.path
                        );
                        context.add_scalar_constant_binding(
                            &value.output,
                            press,
                            press_binding,
                            on,
                            off,
                        );
                    }
                    continue;
                }

                // The action is 1 while the input is pressed, like a button.
                match button_press_input(path, None, validate_path) {
                    // A float action bound to a boolean input reads 0 or 1.
                    Some((press, true)) => {
                        context.try_suggest_binding::<bool>(value.output.path.clone(), press)
                    }
                    Some((press, false)) => bind_threshold(
                        &value.output,
                        press,
                        None,
                        action_set_name,
                        action_set,
                        context,
                    ),
                    None => InvalidActionPath(path, &value.output.path).report(context),
                }
            }
            ActionBinding::ForceSensor(data) => {
                let Some(ValidActionBindingData {
                    path,
                    inputs: ForceSensorInput { force },
                    ..
                }) = data.validate_path(context)
                else {
                    continue;
                };

                let _ = force
                    .try_bind_with_component(path, context, validate_path)
                    .inspect_err(|e| e.report(context));
            }
            ActionBinding::Grab(data) => {
                let Some(ValidActionBindingData {
                    path,
                    inputs: GrabInput { grab },
                    parameters,
                }) = data.validate_path(context)
                else {
                    continue;
                };

                let value_path = path.with_component(DynComponent::Value);
                let Some(value_path) = validate_path(value_path) else {
                    InvalidActionPath(value_path, &grab.output.path).report(context);
                    continue;
                };
                // Controllers without a force sensor grab by value alone.
                let force_path = validate_path(path.with_component(DynComponent::Force))
                    .filter(|p| p.component == Some(DynComponent::Force));

                if !context.accepts_custom_binding(&grab.output) {
                    continue;
                }
                let force_action = match force_path {
                    Some(force_path) => {
                        let Some(action) =
                            context.input_action(action_set_name, action_set, force_path)
                        else {
                            continue;
                        };
                        Some(action)
                    }
                    None => None,
                };
                let Some(value_action) =
                    context.input_action(action_set_name, action_set, value_path)
                else {
                    continue;
                };

                trace!(
                    "suggesting {value_path} (force: {}) for {} (grab binding)",
                    force_path.is_some(),
                    grab.output.path
                );
                // Only a grab that the force held drops for moving.
                let speeds = match force_action {
                    Some(_) => context.hand_speeds(),
                    None => Default::default(),
                };
                context.add_custom_binding(
                    &grab.output,
                    path.hand,
                    value_path,
                    BoolBindingType::Grab(GrabBindingData::new(
                        force_action,
                        value_action,
                        parameters,
                        speeds,
                        path.hand,
                    )),
                );
            }
            ActionBinding::Scroll(data) => {
                let Some(ValidActionBindingData {
                    inputs: ScrollInput { scroll },
                    path,
                    parameters,
                }) = data.validate_path(context)
                else {
                    continue;
                };
                let Some(path) = validate_path(path) else {
                    InvalidActionPath(path, &scroll.output.path).report(context);
                    continue;
                };
                let input = match path.subpath {
                    paths::DynSubpath::Trackpad => custom_bindings::ScrollSource::Trackpad,
                    _ => custom_bindings::ScrollSource::Joystick,
                };
                let touch = (input == custom_bindings::ScrollSource::Trackpad)
                    .then(|| validate_path(path.with_component(DynComponent::Touch)))
                    .flatten()
                    .filter(|touch| touch.component == Some(DynComponent::Touch))
                    .and_then(|touch| context.input_action(action_set_name, action_set, touch));
                let smooth = parameters
                    .and_then(|p| p.scroll_mode.as_deref())
                    .is_some_and(|mode| mode == "smooth");
                let multiplier = parameters
                    .and_then(|p| p.smooth_scroll_multiplier.as_deref().copied())
                    .unwrap_or(1.0);
                context.add_vector2_binding(
                    &scroll.output,
                    path,
                    Vector2Transform::Scroll(custom_bindings::ScrollData::new(
                        input, smooth, multiplier, touch,
                    )),
                    action_set_name,
                    action_set,
                );
            }
            ActionBinding::Trackpad(data) | ActionBinding::Joystick(data) => {
                let Some(ValidActionBindingData {
                    path,
                    inputs,
                    parameters,
                }) = data.validate_path(context)
                else {
                    continue;
                };

                let Vector2Input {
                    position,
                    click,
                    touch,
                } = inputs;

                if let Some(click) = click {
                    match parameters.and_then(Vector2Parameters::sticky_thresholds) {
                        Some(sticky) => bind_sticky_click(
                            click,
                            path,
                            &sticky,
                            validate_path,
                            action_set_name,
                            action_set,
                            context,
                        ),
                        None => {
                            let _ = click.try_bind_with_component(path, context, validate_path);
                        }
                    }
                }

                if let Some(touch) = touch {
                    let _ = touch.try_bind_with_component(path, context, validate_path);
                }

                if let Some(position) = position {
                    let transform = parameters.map(Vector2Parameters::transform);
                    match transform.filter(|t| !t.is_identity()) {
                        None => {
                            let _ = position.try_bind_with_component(path, context, validate_path);
                        }
                        Some(transform) => {
                            if let Some(path) = validate_path(path) {
                                context.add_vector2_binding(
                                    &position.output,
                                    path,
                                    transform,
                                    action_set_name,
                                    action_set,
                                );
                            } else {
                                InvalidActionPath(path, &position.output.path).report(context);
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn handle_chord_bindings(
    validate_path: &dyn PathValidator,
    context: &mut BindingsProfileLoadContext,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    chords: &[ChordBinding],
) {
    'chords: for ChordBinding { output, inputs } in chords {
        let Some((MaybeInputPath::Valid(last), _)) = inputs.last() else {
            warn!("Ignoring chord without inputs for {}", output.path);
            continue;
        };
        if !context.accepts_custom_binding(output) {
            continue;
        }

        let mut parts = Vec::with_capacity(inputs.len());
        for (path, slot) in inputs {
            let path = match path {
                MaybeInputPath::Valid(path) => *path,
                MaybeInputPath::Invalid { path, error } => {
                    context.unusable_binding(format_args!(
                        "ignoring chord for {}: invalid input path {path}: {error}",
                        output.path
                    ));
                    continue 'chords;
                }
            };
            let part = match slot.as_str() {
                "click" | "single" | "long" | "held" | "double" => {
                    button_press_input(path, None, validate_path).and_then(|(press, _)| {
                        threshold_binding(output, press, None, action_set_name, action_set, context)
                    })
                }
                "touch" => touch_input(path, validate_path).and_then(|(touch, params)| {
                    threshold_binding(
                        output,
                        touch,
                        Some(&params),
                        action_set_name,
                        action_set,
                        context,
                    )
                }),
                direction @ ("north" | "east" | "south" | "west" | "center") => {
                    let direction = match direction {
                        "north" => DpadDirection::North,
                        "east" => DpadDirection::East,
                        "south" => DpadDirection::South,
                        "west" => DpadDirection::West,
                        _ => DpadDirection::Center,
                    };
                    // Directions count without a click or touch, like a plain stick push. The
                    // center is where an untouched input rests though, so it needs a click.
                    let click =
                        matches!(direction, DpadDirection::Center).then(|| DpadParameters {
                            sub_mode: DpadSubMode::Click,
                            ..Default::default()
                        });
                    validate_path(path)
                        .and_then(|parent| {
                            context.get_dpad_actions(
                                validate_path,
                                parent,
                                action_set_name,
                                action_set,
                                click.as_ref(),
                            )
                        })
                        .filter(|actions| click.is_none() || actions.click_or_touch.is_some())
                        .map(|actions| {
                            let zones = custom_bindings::DpadZoneState::new(
                                custom_bindings::DpadZones::default(),
                            );
                            BoolBindingType::Dpad(DpadData::new(actions, direction, zones.into()))
                        })
                }
                other => {
                    warn!("Ignoring chord for {}: unknown input {other}", output.path);
                    continue 'chords;
                }
            };
            let Some(part) = part else {
                context.unusable_binding(format_args!(
                    "ignoring chord for {}: no {slot} on {path}",
                    output.path
                ));
                continue 'chords;
            };
            parts.push((context.hand_path(path.hand), part));
        }

        trace!("binding chord for {}", output.path);
        // Owned by the hand of the last input, which usually completes the chord.
        context.add_custom_binding(
            output,
            last.hand,
            *last,
            BoolBindingType::Chord(ChordData::new(parts)),
        );
    }
}

pub fn handle_skeleton_bindings(
    context: &BindingsProfileLoadContext,
    bindings: &[SimpleActionBinding],
) {
    for SimpleActionBinding { output, path } in bindings {
        trace!("binding skeleton action {} to {path:?}", output.path);
        if !context.find_action(&output.path) {
            continue;
        };

        let ActionData::Skeleton(hand) = &context.actions[&output.path] else {
            warn!(
                "Expected skeleton action for skeleton binding {}, skipping",
                output.path
            );
            continue;
        };
        let bound_hand = match path.as_str() {
            "/user/hand/left/input/skeleton/left" => Hand::Left,
            "/user/hand/right/input/skeleton/right" => Hand::Right,
            other => {
                warn!(
                    "Got invalid skeleton binding {other} for action {}",
                    output.path
                );
                continue;
            }
        };

        if bound_hand != *hand {
            warn!(
                "Action {} was created with hand {hand:?}, but is bound to hand {bound_hand:?}",
                output.path
            );
        }
    }
}

pub fn handle_haptic_bindings(
    instance: &xr::Instance,
    context: &mut BindingsProfileLoadContext,
    bindings: &[SimpleActionBinding],
) {
    for SimpleActionBinding { output, path } in bindings {
        if !matches!(
            path.as_str(),
            "/user/hand/left/output/haptic" | "/user/hand/right/output/haptic",
        ) {
            warn!("invalid haptic path {path} for {}", output.path);
            continue;
        };
        if !context.find_action(&output.path) {
            continue;
        };

        if !matches!(&context.actions[&output.path], ActionData::Haptic(_)) {
            warn!(
                "Expected haptic action for haptic binding {path}, got {}, skipping",
                output.path
            );
            continue;
        }
        let xr_path = instance.string_to_path(path).unwrap();
        context.push_binding(output.path.clone(), xr_path);
    }
}

/// SteamVR's haptics for clicks that come from an analog input crossing a threshold: a pulse on
/// the press with the `haptic_*` parameters, and on the release with the `off_haptic_*` ones. They
/// read the inputs of [handle_sources]' bindings on their own, as SteamVR plays them whether or
/// not the game reads the action. Physical clicks have none.
pub fn handle_binding_haptics(
    validate_path: &dyn PathValidator,
    context: &mut BindingsProfileLoadContext,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    sources: &[ActionBinding],
) {
    use DynComponent::{Click, Force, Value};
    let is_bool = |context: &BindingsProfileLoadContext, output: &ActionPath| {
        context
            .actions
            .get(&output.path)
            .is_some_and(<bool as context::WithActionPattern>::check_match)
    };
    let analog_click = |path: DynInputPath| {
        validate_path(path.with_component(Click))
            .filter(|input| matches!(input.component, Some(Value | Force)))
    };

    for source in sources {
        let (output, path, haptic, press) = match source {
            ActionBinding::Button(ActionBindingData {
                path: MaybeInputPath::Valid(path),
                inputs:
                    ButtonInput {
                        click,
                        grab,
                        long,
                        held,
                        single,
                        ..
                    },
                parameters,
            }) => {
                let Some(output) = [click, grab, long, held, single]
                    .into_iter()
                    .flatten()
                    .find(|output| is_bool(context, &output.output))
                else {
                    continue;
                };
                let force_input = parameters.as_ref().and_then(|p| p.force_input);
                let Some((press, false)) = button_press_input(*path, force_input, validate_path)
                else {
                    continue;
                };
                let thresholds = parameters.as_ref().map(|p| &p.click_threshold);
                let haptic = thresholds
                    .map_or_else(HapticParameters::default, |t| t.haptic)
                    .haptic(BindingHaptic::DEFAULT_AMPLITUDE);
                let Some(haptic) = haptic else {
                    continue;
                };
                let Some(press) =
                    threshold_press(press, thresholds, action_set_name, action_set, context)
                else {
                    continue;
                };
                (&output.output, path, haptic, BindingPress::Custom(press))
            }
            ActionBinding::ToggleButton(ActionBindingData {
                path: MaybeInputPath::Valid(path),
                inputs:
                    ButtonInput {
                        click: Some(output),
                        ..
                    },
                parameters,
            }) if is_bool(context, &output.output) => {
                let Some(input) = analog_click(*path) else {
                    continue;
                };
                let haptic = parameters
                    .as_ref()
                    .map_or_else(HapticParameters::default, |p| p.click_threshold.haptic)
                    .haptic(BindingHaptic::DEFAULT_AMPLITUDE);
                let (Some(haptic), Some(action)) = (
                    haptic,
                    context.input_action(action_set_name, action_set, input),
                ) else {
                    continue;
                };
                (&output.output, path, haptic, BindingPress::Runtime(action))
            }
            // The default is unclear: SteamVR's legacy Touch bindings set haptic_amplitude 0 here,
            // but games that turn it off in button mode leave trigger mode alone. Only bindings
            // that set one get it.
            ActionBinding::Trigger(ActionBindingData {
                path: MaybeInputPath::Valid(path),
                inputs:
                    TriggerInput {
                        click: Some(output),
                        ..
                    },
                parameters: Some(parameters),
            }) if is_bool(context, &output.output) => {
                let (Some(input), Some(haptic)) =
                    (analog_click(*path), parameters.haptic.haptic(0.0))
                else {
                    continue;
                };
                let press = if parameters.click_activate_threshold.is_some()
                    || parameters.click_deactivate_threshold.is_some()
                {
                    threshold_press(
                        input,
                        Some(parameters),
                        action_set_name,
                        action_set,
                        context,
                    )
                    .map(BindingPress::Custom)
                } else {
                    context
                        .input_action(action_set_name, action_set, input)
                        .map(BindingPress::Runtime)
                };
                let Some(press) = press else {
                    continue;
                };
                (&output.output, path, haptic, press)
            }
            // Off by default in the binding UI.
            ActionBinding::Grab(ActionBindingData {
                path: MaybeInputPath::Valid(path),
                inputs: GrabInput { grab: output },
                parameters: Some(parameters),
            }) if is_bool(context, &output.output) => {
                let Some(haptic) = parameters.haptic.haptic(0.0) else {
                    continue;
                };
                let Some(value_path) = validate_path(path.with_component(Value)) else {
                    continue;
                };
                let force_path = validate_path(path.with_component(Force))
                    .filter(|p| p.component == Some(Force));
                let force_action = match force_path {
                    Some(force_path) => {
                        match context.input_action(action_set_name, action_set, force_path) {
                            Some(action) => Some(action),
                            None => continue,
                        }
                    }
                    None => None,
                };
                let Some(value_action) =
                    context.input_action(action_set_name, action_set, value_path)
                else {
                    continue;
                };
                let speeds = match force_action {
                    Some(_) => context.hand_speeds(),
                    None => Default::default(),
                };
                let grab = GrabBindingData::new(
                    force_action,
                    value_action,
                    Some(parameters),
                    speeds,
                    path.hand,
                );
                let press =
                    BoolBindingData::new(BoolBindingType::Grab(grab), context.hand_path(path.hand));
                (&output.output, path, haptic, BindingPress::Custom(press))
            }
            _ => continue,
        };
        trace!(
            "haptic for {} on {path} ({action_set_name}): {haptic:?}",
            output.path
        );
        context.add_binding_haptic(action_set_name, output, path.hand, press, haptic);
    }
}

/// A click at `params`' thresholds on an analog input, or on a 2D one by its distance from the
/// center.
fn threshold_press(
    input: DynInputPath,
    params: Option<&ClickThresholdParams>,
    action_set_name: &str,
    action_set: &xr::ActionSet,
    context: &mut BindingsProfileLoadContext,
) -> Option<BoolBindingData> {
    let ty = if input.component.is_none() {
        let action = context.input_action(action_set_name, action_set, input)?;
        BoolBindingType::ThresholdVec2(ThresholdBindingData::new(action, params))
    } else {
        let action = context.input_action(action_set_name, action_set, input)?;
        BoolBindingType::ThresholdFloat(ThresholdBindingData::new(action, params))
    };
    Some(BoolBindingData::new(ty, context.hand_path(input.hand)))
}

pub fn handle_pose_bindings(context: &mut BindingsProfileLoadContext, bindings: &[PoseBinding]) {
    for PoseBinding {
        output,
        path: (hand, pose_ty),
    } in bindings
    {
        if !context.find_action(&output.path) {
            continue;
        };

        if !matches!(&context.actions[&output.path], ActionData::Pose) {
            warn!(
                "Expected pose action for pose binding on {}, skipping",
                output.path
            );
            continue;
        }

        let bound = context
            .pose_bindings
            .entry(output.path.clone())
            .or_default();

        let b = match hand {
            Hand::Left => &mut bound.left,
            Hand::Right => &mut bound.right,
        };
        *b = Some(*pose_ty);
        trace!(
            "bound {:?} to pose {} for hand {hand:?}",
            *pose_ty, output.path
        );
    }
}

/// The output bound to each slot (input) of a mode.
type SlotOutputs<'a> = Vec<(&'static str, &'a ActionPath)>;

impl ActionBinding {
    /// The mode's name, the input path and the bound output of each slot, as
    /// GetActionBindingInfo reports them.
    fn slots(&self) -> Option<(&'static str, DynInputPath, SlotOutputs<'_>)> {
        fn slot<'a, C>(
            name: &'static str,
            output: &'a Option<ActionBindingOutput<C>>,
        ) -> Option<(&'static str, &'a ActionPath)> {
            output.as_ref().map(|output| (name, &output.output))
        }
        fn directions(inputs: &DpadInput) -> Vec<Option<(&'static str, &ActionPath)>> {
            vec![
                slot("north", &inputs.north),
                slot("east", &inputs.east),
                slot("south", &inputs.south),
                slot("west", &inputs.west),
                slot("center", &inputs.center),
            ]
        }
        let (mode, path, slots) = match self {
            // What SteamVR reports for radial menu buttons isn't known.
            Self::None(_) | Self::Radial(_) | Self::RadialMenu(_) => return None,
            Self::Button(data) => {
                let inputs = &data.inputs;
                let slots = vec![
                    slot("click", &inputs.click),
                    slot("touch", &inputs.touch),
                    slot("double", &inputs.double),
                    slot("grab", &inputs.grab),
                    slot("long", &inputs.long),
                    slot("held", &inputs.held),
                    slot("single", &inputs.single),
                ];
                ("button", &data.path, slots)
            }
            Self::ToggleButton(data) => {
                let slots = vec![
                    slot("click", &data.inputs.click),
                    slot("touch", &data.inputs.touch),
                ];
                ("toggle_button", &data.path, slots)
            }
            Self::Dpad(data) => ("dpad", &data.path, directions(&data.inputs)),
            Self::DpadClick(data) => ("dpad_click", &data.path, directions(&data.inputs)),
            Self::DpadTouch(data) => ("dpad_touch", &data.path, directions(&data.inputs)),
            Self::Trigger(data) => {
                let inputs = &data.inputs;
                let slots = vec![
                    slot("pull", &inputs.pull),
                    slot("touch", &inputs.touch),
                    slot("click", &inputs.click),
                ];
                ("trigger", &data.path, slots)
            }
            Self::ScalarConstant(data) => (
                "scalar_constant",
                &data.path,
                vec![Some(("value", &data.inputs.value.output))],
            ),
            Self::ForceSensor(data) => (
                "force_sensor",
                &data.path,
                vec![Some(("force", &data.inputs.force.output))],
            ),
            Self::Grab(data) => (
                "grab",
                &data.path,
                vec![Some(("grab", &data.inputs.grab.output))],
            ),
            Self::Scroll(data) => (
                "scroll",
                &data.path,
                vec![Some(("scroll", &data.inputs.scroll.output))],
            ),
            Self::Trackpad(data) | Self::Joystick(data) => {
                let mode = match self {
                    Self::Trackpad(_) => "trackpad",
                    _ => "joystick",
                };
                let inputs = &data.inputs;
                let slots = vec![
                    slot("position", &inputs.position),
                    slot("click", &inputs.click),
                    slot("touch", &inputs.touch),
                ];
                (mode, &data.path, slots)
            }
        };
        let MaybeInputPath::Valid(path) = path else {
            return None;
        };
        Some((mode, *path, slots.into_iter().flatten().collect()))
    }
}

/// Whether the profile `validate_path` checks for has what `slot` of a mode reads on `path`.
fn has_slot(validate_path: &dyn PathValidator, path: DynInputPath, slot: &str) -> bool {
    use DynComponent::*;
    let components: &[Option<DynComponent>] = match slot {
        // Touches fall back to the pull, see touch_input.
        "touch" => &[Some(Touch), Some(Value)],
        "pull" | "grab" => &[Some(Value)],
        "force" => &[Some(Force)],
        "position" | "scroll" | "north" | "east" | "south" | "west" | "center" => &[None],
        _ => &[Some(Click), Some(Value), Some(Force)],
    };
    components
        .iter()
        .any(|&component| validate_path(DynInputPath { component, ..path }).is_some())
}

/// Records what GetActionBindingInfo reports for the actions this profile got bindings for:
/// the OpenVR input, mode and slot of each of their sources, chord inputs and haptic outputs
/// that the profile has.
pub fn record_binding_info(
    validate_path: &dyn PathValidator,
    controller_type: &str,
    context: &mut BindingsProfileLoadContext,
    bindings: &HashMap<String, ActionSetBinding>,
) {
    use crate::input::origins::{self, BindingInfo};

    let info = |path: DynInputPath, mode, slot| {
        let analog = validate_path(path.with_component(DynComponent::Value))
            .is_some_and(|path| path.component == Some(DynComponent::Value));
        BindingInfo {
            hand: path.hand,
            input: origins::openvr_input(controller_type, path.subpath),
            mode,
            slot,
            source_type: origins::source_type(path.subpath, analog),
        }
    };

    let mut found: Vec<(&ActionPath, BindingInfo)> = Vec::new();
    for (set_name, set) in bindings {
        if context.get_action_set(set_name).is_none() {
            continue;
        }
        for source in &set.sources {
            let Some((mode, path, slots)) = source.slots() else {
                continue;
            };
            for (slot, output) in slots {
                if has_slot(validate_path, path, slot) {
                    found.push((output, info(path, mode, slot)));
                }
            }
        }
        for chord in &set.chords {
            for (path, slot) in &chord.inputs {
                let MaybeInputPath::Valid(path) = path else {
                    continue;
                };
                // Each input as the mode a chord reads it with.
                let (mode, slot) = match slot.as_str() {
                    "click" => ("button", "click"),
                    "touch" => ("button", "touch"),
                    "single" => ("button", "single"),
                    "long" => ("button", "long"),
                    "held" => ("button", "held"),
                    "double" => ("button", "double"),
                    "north" => ("dpad", "north"),
                    "east" => ("dpad", "east"),
                    "south" => ("dpad", "south"),
                    "west" => ("dpad", "west"),
                    "center" => ("dpad", "center"),
                    _ => continue,
                };
                if has_slot(validate_path, *path, slot) {
                    found.push((&chord.output, info(*path, mode, slot)));
                }
            }
        }
        for SimpleActionBinding { output, path } in &set.haptics {
            let hand = match path.as_str() {
                "/user/hand/left/output/haptic" => Hand::Left,
                "/user/hand/right/output/haptic" => Hand::Right,
                _ => continue,
            };
            let info = BindingInfo {
                hand,
                input: "/output/haptic".to_owned(),
                mode: "",
                slot: "",
                source_type: "vibration",
            };
            found.push((output, info));
        }
    }

    for (output, info) in found {
        // Only actions that got a binding, so no handles are made for unknown names.
        if !context.input_paths_out.contains_key(&output.path) {
            continue;
        }
        let infos = context
            .binding_info_out
            .entry(output.path.clone())
            .or_default();
        if !infos.contains(&info) {
            infos.push(info);
        }
    }
}
