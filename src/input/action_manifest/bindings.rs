#![allow(private_interfaces)]

use super::context::BindingsProfileLoadContext;
use crate::input::action_manifest::context;
use crate::input::profiles::paths::DynComponent;
use crate::input::profiles::{Component, DynInputPath, paths};
use crate::input::{ActionData, BoundPoseType};
use crate::{
    input::custom_bindings::{
        self, BoolBindingType, ChordData, DoubleTapData, DpadData, DpadDirection, GrabBindingData,
        ThresholdBindingData, TimedPress, TimedPressData, ToggleData, Vector2Inversion,
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
use std::time::Duration;

/**
 * Structure for binding files
 */

#[derive(Deserialize)]
pub struct Bindings {
    #[serde(deserialize_with = "lenient_sets")]
    pub bindings: HashMap<String, ActionSetBinding>,
}

// SteamVR skips what it doesn't understand, so one odd entry (an unknown mode, a missing
// field, a tracker pose path...) only drops that entry rather than the whole file.
#[derive(Deserialize)]
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
#[derive(Deserialize)]
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

#[derive(Debug)]
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

#[derive(Deserialize)]
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

#[derive(Deserialize)]
pub struct SimpleActionBinding {
    output: ActionPath,
    path: String,
}

/// Note that when this is used, it's typically missing the final component
#[derive(Deserialize)]
#[serde(from = "String")]
enum MaybeInputPath {
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

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionBinding {
    None(IgnoredAny),
    Button(ActionBindingData<ButtonInput, ButtonParameters>),
    ToggleButton(ActionBindingData<ButtonInput>),
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
}

#[derive(Deserialize)]
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
    fn validate_path(&self) -> Option<ValidActionBindingData<'_, Inputs, Parameters>> {
        match &self.path {
            MaybeInputPath::Valid(path) => Some(ValidActionBindingData {
                path: *path,
                inputs: &self.inputs,
                parameters: self.parameters.as_ref(),
            }),
            MaybeInputPath::Invalid { path, error } => {
                warn!("got invalid input path {path} - {error}");
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

/// Marker for actions that should be bound to a custom binding
#[derive(Debug)]
struct Custom;

struct InvalidActionPath<'a>(DynInputPath, &'a str);
impl InvalidActionPath<'_> {
    fn warn(&self) {
        warn!("invalid path {} for {}", self.0, self.1);
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

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let value = T::deserialize(serde::de::value::F64Deserializer::<E>::new(value))
                    .map_err(E::custom)?;
                Ok(StringOrTyped(value))
            }
        }

        deserializer.deserialize_any(StringOrTypedVisitor(PhantomData))
    }
}

#[derive(Deserialize)]
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

#[derive(Deserialize, Default)]
pub struct ClickThresholdParams {
    pub click_activate_threshold: Option<StringOrTyped<f32>>,
    pub click_deactivate_threshold: Option<StringOrTyped<f32>>,
}

impl ClickThresholdParams {
    fn new_for_touch_conversion() -> Self {
        Self {
            click_activate_threshold: Some(0.01f32.into()),
            click_deactivate_threshold: Some(0.005f32.into()),
        }
    }
}

#[derive(Deserialize)]
struct ScalarConstantParameters {
    #[serde(rename = "on/x")]
    #[allow(unused)]
    on_x: Option<String>,
}

#[derive(Deserialize)]
struct ButtonParameters {
    #[serde(default, deserialize_with = "ButtonForceInput::default_deserialize")]
    force_input: Option<ButtonForceInput>,
    /// Seconds
    long_press_delay: Option<StringOrTyped<f32>>,
    #[serde(flatten)]
    click_threshold: ClickThresholdParams,
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

#[derive(Deserialize, Debug)]
struct DpadInput {
    east: Option<ActionBindingOutput<Custom>>,
    south: Option<ActionBindingOutput<Custom>>,
    north: Option<ActionBindingOutput<Custom>>,
    west: Option<ActionBindingOutput<Custom>>,
    center: Option<ActionBindingOutput<Custom>>,
}

#[derive(Deserialize)]
#[serde(default)]
pub struct DpadParameters {
    pub sub_mode: DpadSubMode,
    pub deadzone_pct: StringOrTyped<f32>,
    pub overlap_pct: StringOrTyped<f32>,
    pub sticky: StringOrTyped<bool>,
}

impl Default for DpadParameters {
    fn default() -> Self {
        Self {
            sub_mode: DpadSubMode::Touch,
            deadzone_pct: StringOrTyped(50.0),
            overlap_pct: StringOrTyped(50.0),
            sticky: StringOrTyped(false),
        }
    }
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum DpadSubMode {
    Click,
    Touch,
}

#[derive(Deserialize)]
struct TriggerInput {
    pull: Option<ActionBindingOutput<paths::Value>>,
    touch: Option<ActionBindingOutput<paths::Touch>>,
    click: Option<ActionBindingOutput<paths::Click>>,
}

#[derive(Deserialize)]
struct ScalarConstantInput {
    value: ActionBindingOutput<paths::Value>,
}

#[derive(Deserialize)]
struct ForceSensorInput {
    force: ActionBindingOutput<paths::Force>,
}

#[derive(Deserialize)]
struct ForceSensorParameters {
    #[allow(unused)]
    haptic_amplitude: Option<String>,
}

#[derive(Deserialize)]
struct GrabInput {
    grab: ActionBindingOutput<Custom>,
}

#[derive(Deserialize)]
pub struct GrabParameters {
    pub value_hold_threshold: Option<StringOrTyped<f32>>,
    pub value_release_threshold: Option<StringOrTyped<f32>>,
}

#[derive(Deserialize)]
struct ScrollInput {
    scroll: ActionBindingOutput<()>,
}

#[derive(Deserialize)]
struct ScrollParameters {
    /// "discrete" (default) or "smooth"
    scroll_mode: Option<String>,
    smooth_scroll_multiplier: Option<StringOrTyped<f32>>,
}

#[derive(Deserialize)]
struct Vector2Input {
    position: Option<ActionBindingOutput<()>>,
    click: Option<ActionBindingOutput<paths::Click>>,
    touch: Option<ActionBindingOutput<paths::Touch>>,
}

#[derive(Deserialize)]
struct Vector2Parameters {
    #[serde(default)]
    invert: Vector2Inversion,
    deadzone_pct: Option<StringOrTyped<f32>>,
    maxzone_pct: Option<StringOrTyped<f32>>,
    #[allow(unused)]
    sticky_click: Option<StringOrTyped<bool>>,
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
    // TODO: actually take the deadzone and overlap into account
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

    for (path, direction) in bound_actions {
        context.add_custom_binding(
            path,
            parent_path.hand,
            parent_path,
            BoolBindingType::Dpad(DpadData::new(actions.clone(), direction)),
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
        InvalidActionPath(path.with_component(DynComponent::Touch), &touch.output.path).warn();
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
            ActionBinding::ToggleButton(data) => {
                let Some(ValidActionBindingData {
                    path,
                    inputs: ButtonInput { touch, click, .. },
                    parameters: _,
                }) = data.validate_path()
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
                        InvalidActionPath(click_path, &click.output.path).warn();
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
                }) = data.validate_path()
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
                        .inspect_err(InvalidActionPath::warn)
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
                        InvalidActionPath(click_path, &output.output.path).warn();
                    }
                    continue;
                };
                let thresholds = parameters.map(|b| &b.click_threshold);

                // A single press only has to wait when it could turn into something else.
                let long_delay = long.as_ref().or(held.as_ref()).map(|_| {
                    parameters
                        .and_then(|p| p.long_press_delay.as_deref().copied())
                        .filter(|delay| delay.is_finite() && *delay >= 0.0)
                        .map_or(
                            TimedPressData::DEFAULT_LONG_PRESS_DELAY,
                            Duration::from_secs_f32,
                        )
                });
                let single_waits = double.is_some() || long_delay.is_some();
                let immediate = [
                    click.as_ref(),
                    grab.as_ref(),
                    single.as_ref().filter(|_| !single_waits),
                ];
                for click in immediate.into_iter().flatten() {
                    if is_click {
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
                    let Some(press_binding) = threshold_binding(
                        &output.output,
                        press,
                        thresholds,
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
                }) = data.validate_path()
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
                    InvalidActionPath(path, &format!("{inputs:#?}")).warn();
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
                }) = data.validate_path()
                else {
                    continue;
                };

                if let Some(pull) = pull {
                    let _ = pull
                        .try_bind_with_component(path, context, validate_path)
                        .inspect_err(InvalidActionPath::warn);
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
                        None => InvalidActionPath(click_path, &click.output.path).warn(),
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
                    ..
                }) = data.validate_path()
                else {
                    continue;
                };

                let _ = value
                    .try_bind_with_component(path, context, validate_path)
                    .inspect_err(InvalidActionPath::warn);
            }
            ActionBinding::ForceSensor(data) => {
                let Some(ValidActionBindingData {
                    path,
                    inputs: ForceSensorInput { force },
                    ..
                }) = data.validate_path()
                else {
                    continue;
                };

                let _ = force
                    .try_bind_with_component(path, context, validate_path)
                    .inspect_err(InvalidActionPath::warn);
            }
            ActionBinding::Grab(data) => {
                let Some(ValidActionBindingData {
                    path,
                    inputs: GrabInput { grab },
                    parameters,
                }) = data.validate_path()
                else {
                    continue;
                };

                let value_path = path.with_component(DynComponent::Value);
                let Some(value_path) = validate_path(value_path) else {
                    InvalidActionPath(value_path, &grab.output.path).warn();
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
                context.add_custom_binding(
                    &grab.output,
                    path.hand,
                    value_path,
                    BoolBindingType::Grab(GrabBindingData::new(
                        force_action,
                        value_action,
                        parameters,
                    )),
                );
            }
            ActionBinding::Scroll(data) => {
                let Some(ValidActionBindingData {
                    inputs: ScrollInput { scroll },
                    path,
                    parameters,
                }) = data.validate_path()
                else {
                    continue;
                };
                let Some(path) = validate_path(path) else {
                    InvalidActionPath(path, &scroll.output.path).warn();
                    continue;
                };
                let input = match path.subpath {
                    paths::DynSubpath::Trackpad => custom_bindings::ScrollInput::Trackpad,
                    _ => custom_bindings::ScrollInput::Joystick,
                };
                let touch = (input == custom_bindings::ScrollInput::Trackpad)
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
                }) = data.validate_path()
                else {
                    continue;
                };

                let Vector2Input {
                    position,
                    click,
                    touch,
                } = inputs;

                if let Some(click) = click {
                    let _ = click.try_bind_with_component(path, context, validate_path);
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
                                InvalidActionPath(path, &position.output.path).warn();
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
                    warn!(
                        "Ignoring chord for {}: invalid path {path} - {error}",
                        output.path
                    );
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
                    // Directions count without a click or touch, like a plain stick push.
                    validate_path(path)
                        .and_then(|parent| {
                            context.get_dpad_actions(
                                validate_path,
                                parent,
                                action_set_name,
                                action_set,
                                None,
                            )
                        })
                        .map(|actions| BoolBindingType::Dpad(DpadData::new(actions, direction)))
                }
                other => {
                    warn!("Ignoring chord for {}: unknown input {other}", output.path);
                    continue 'chords;
                }
            };
            let Some(part) = part else {
                warn!(
                    "Ignoring chord for {}: no {slot} on {path} for this controller",
                    output.path
                );
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
