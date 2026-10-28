//! The components of SteamVR's render models, from the JSON next to each model: a mesh per part,
//! its local frame, how it moves with the controller's input and when it shows.
//!
//! The JSON isn't documented. The fields are read as SteamVR's own models use them, with the
//! OpenVR device frame (x right, y up, z backward, meters) and `rotate_xyz` degrees applied as
//! Z·Y·X, which is the order that puts a trackpad's `component_local` -Z along its normal.

use crate::input::{ComponentInput, DynSubpath};
use glam::{Mat4, Vec2, Vec3};
use openvr as vr;
use serde::Deserialize;
use std::collections::BTreeMap;

/// A model's JSON, as far as it's used.
#[derive(Deserialize)]
struct ModelJson {
    thumbnail: Option<String>,
    #[serde(default)]
    components: BTreeMap<String, ComponentJson>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ComponentJson {
    filename: Option<String>,
    component_local: Option<FrameJson>,
    motion: Option<MotionJson>,
    visibility: Option<Visibility>,
}

#[derive(Deserialize)]
struct FrameJson {
    #[serde(default)]
    origin: [f32; 3],
    #[serde(default)]
    rotate_xyz: [f32; 3],
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MotionJson {
    #[serde(rename = "type")]
    ty: String,
    trigger_path: Option<String>,
    pressed_path: Option<String>,
    component_path: Option<String>,
    joystick_path: Option<String>,
    touched_path: Option<String>,
    x_path: Option<String>,
    dpad_up_path: Option<String>,
    dpad_down_path: Option<String>,
    dpad_left_path: Option<String>,
    dpad_right_path: Option<String>,
    value_mapping: Option<[f32; 2]>,
    pressed_value_override: Option<f32>,
    pivot: Option<[f32; 3]>,
    axis: Option<[f32; 3]>,
    center: Option<[f32; 3]>,
    rotate_xyz: Option<[f32; 3]>,
    joystick_rotation_x: Option<[f32; 2]>,
    joystick_rotation_y: Option<[f32; 2]>,
    press_rotation_x: Option<[f32; 2]>,
    press_rotation_y: Option<[f32; 2]>,
    press_translate: Option<[f32; 3]>,
    touch_translate_x: Option<[f32; 2]>,
    touch_translate_y: Option<[f32; 2]>,
}

/// When a component shows: in scroll mode, then while pressed, then while touched, else by
/// default (shown unless the JSON says otherwise).
#[derive(Deserialize, Default, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub(super) struct Visibility {
    pub default: Option<bool>,
    pub touch: Option<bool>,
    pub press: Option<bool>,
    pub scroll: Option<bool>,
}

impl Visibility {
    fn visible(&self, touched: bool, pressed: bool, scroll: bool) -> bool {
        if scroll && let Some(visible) = self.scroll {
            return visible;
        }
        if pressed && let Some(visible) = self.press {
            return visible;
        }
        if touched && let Some(visible) = self.touch {
            return visible;
        }
        self.default.unwrap_or(true)
    }
}

pub(super) struct ModelInfo {
    pub thumbnail: Option<String>,
    /// In name order, which is what the component indices count.
    pub components: Vec<(String, Component)>,
}

impl ModelInfo {
    pub fn parse(json: &[u8]) -> Result<Self, serde_json::Error> {
        // SteamVR's Vive tracker files end in a stray brace, which it tolerates.
        let model: ModelJson = serde_json::Deserializer::from_slice(json)
            .into_iter()
            .next()
            .unwrap_or_else(|| serde_json::from_slice(json))?;
        Ok(Self {
            thumbnail: model.thumbnail,
            components: model
                .components
                .into_iter()
                .map(|(name, component)| (name, Component::from_json(component)))
                .collect(),
        })
    }
}

pub(super) struct Component {
    /// The part's mesh, in the model's space; pose components have none.
    pub filename: Option<String>,
    /// The part's local frame in the model's space, -Z out of its surface.
    pub local: Mat4,
    motion: Option<Motion>,
    pub visibility: Visibility,
}

/// Where a linear motion's value comes from.
enum Source {
    /// `trigger_path` or `component_path`: an analog input, with its click.
    Analog(DynSubpath),
    /// `pressed_path`: a button, 1 while pressed.
    Button(DynSubpath),
    /// `x_path`: an axis of a 2D input, while `touched_path` is touched if it's named.
    Axis {
        subpath: DynSubpath,
        x: bool,
        touched: bool,
    },
}

enum Shape {
    Rotate { pivot: Vec3, axis: Vec3 },
    Translate { axis: Vec3 },
}

/// A stick or a pressed pad tilts in its own frame: about its Z by the input's x, about its X
/// by the input's y, each over `[at -1, at +1]` degrees.
struct Tilt {
    frame: Mat4,
    rotation_x: [f32; 2],
    rotation_y: [f32; 2],
    press_translate: Vec3,
}

enum Motion {
    Linear {
        shape: Shape,
        source: Source,
        /// The angle or distance at 0 and at 1.
        mapping: [f32; 2],
        /// The angle or distance while clicked, instead of the mapping's.
        pressed_override: Option<f32>,
    },
    Stick {
        source: DynSubpath,
        tilt: Tilt,
    },
    Pad {
        source: DynSubpath,
        tilt: Tilt,
        /// Where a touch moves the part along the frame's X and Z, over `[at -1, at +1]`.
        touch_translate: Option<([f32; 2], [f32; 2])>,
    },
    Dpad {
        /// Up, down, left, right.
        buttons: [DynSubpath; 4],
        tilt: Tilt,
    },
}

/// SteamVR's `rotate_xyz` degrees as a rotation.
pub(super) fn rotate_xyz(degrees: [f32; 3]) -> Mat4 {
    Mat4::from_rotation_z(degrees[2].to_radians())
        * Mat4::from_rotation_y(degrees[1].to_radians())
        * Mat4::from_rotation_x(degrees[0].to_radians())
}

fn frame(origin: Option<[f32; 3]>, rotation: Option<[f32; 3]>) -> Mat4 {
    Mat4::from_translation(Vec3::from(origin.unwrap_or_default()))
        * rotate_xyz(rotation.unwrap_or_default())
}

/// The input a SteamVR path like `/input/trigger` or `/input/a/click` names, and its component.
fn parse_path(path: &str) -> Option<(DynSubpath, Option<&str>)> {
    let rest = path.strip_prefix("/input/")?;
    let (subpath, component) = match rest.split_once('/') {
        Some((subpath, component)) => (subpath, Some(component)),
        None => (rest, None),
    };
    Some((DynSubpath::from_openvr_str(subpath)?, component))
}

fn subpath(path: &Option<String>) -> Option<DynSubpath> {
    parse_path(path.as_deref()?).map(|(subpath, _)| subpath)
}

/// Linear interpolation over an axis in [-1, 1].
fn over_axis(range: [f32; 2], axis: f32) -> f32 {
    range[0] + (axis.clamp(-1.0, 1.0) + 1.0) / 2.0 * (range[1] - range[0])
}

impl Tilt {
    fn from_json(motion: &MotionJson) -> Self {
        Self {
            frame: frame(motion.center, motion.rotate_xyz),
            rotation_x: motion
                .joystick_rotation_x
                .or(motion.press_rotation_x)
                .unwrap_or_default(),
            rotation_y: motion
                .joystick_rotation_y
                .or(motion.press_rotation_y)
                .unwrap_or_default(),
            press_translate: Vec3::from(motion.press_translate.unwrap_or_default()),
        }
    }

    fn transform(&self, xy: Vec2, translate: Vec3) -> Mat4 {
        if xy == Vec2::ZERO && translate == Vec3::ZERO {
            return Mat4::IDENTITY;
        }
        self.frame
            * Mat4::from_translation(translate)
            * Mat4::from_rotation_x(over_axis(self.rotation_y, xy.y).to_radians())
            * Mat4::from_rotation_z(over_axis(self.rotation_x, xy.x).to_radians())
            * self.frame.inverse()
    }
}

impl Motion {
    fn from_json(motion: MotionJson) -> Option<Self> {
        let axis = || Some(Vec3::from(motion.axis?).normalize_or_zero());
        let linear = |shape| {
            let source = if let Some(path) = &motion.x_path {
                let (subpath, component) = parse_path(path)?;
                Source::Axis {
                    subpath,
                    x: component == Some("x"),
                    touched: motion.touched_path.is_some(),
                }
            } else if let Some(subpath) =
                subpath(&motion.trigger_path).or_else(|| subpath(&motion.component_path))
            {
                Source::Analog(subpath)
            } else {
                Source::Button(subpath(&motion.pressed_path)?)
            };
            Some(Self::Linear {
                shape,
                source,
                mapping: motion.value_mapping.unwrap_or_default(),
                pressed_override: motion.pressed_value_override,
            })
        };
        match motion.ty.as_str() {
            "rotate" => linear(Shape::Rotate {
                pivot: Vec3::from(motion.pivot.unwrap_or_default()),
                axis: axis()?,
            }),
            "translate" => linear(Shape::Translate { axis: axis()? }),
            "joystick" => Some(Self::Stick {
                source: subpath(&motion.component_path)
                    .or_else(|| subpath(&motion.joystick_path))?,
                tilt: Tilt::from_json(&motion),
            }),
            "trackpad" => Some(Self::Pad {
                source: subpath(&motion.component_path)?,
                tilt: Tilt::from_json(&motion),
                touch_translate: motion.touch_translate_x.zip(motion.touch_translate_y),
            }),
            "dpad" => Some(Self::Dpad {
                buttons: [
                    subpath(&motion.dpad_up_path)?,
                    subpath(&motion.dpad_down_path)?,
                    subpath(&motion.dpad_left_path)?,
                    subpath(&motion.dpad_right_path)?,
                ],
                tilt: Tilt::from_json(&motion),
            }),
            _ => None,
        }
    }

    /// The inputs the motion reads.
    fn subpaths(&self) -> Vec<DynSubpath> {
        match self {
            Self::Linear { source, .. } => vec![match source {
                Source::Analog(subpath) | Source::Button(subpath) => *subpath,
                Source::Axis { subpath, .. } => *subpath,
            }],
            Self::Stick { source, .. } | Self::Pad { source, .. } => vec![*source],
            Self::Dpad { buttons, .. } => buttons.to_vec(),
        }
    }

    /// The part's transform, and whether its input is pressed and touched.
    fn evaluate(&self, read: &dyn Fn(DynSubpath) -> ComponentInput) -> (Mat4, bool, bool) {
        match self {
            Self::Linear {
                shape,
                source,
                mapping,
                pressed_override,
            } => {
                let (value, pressed, touched) = match source {
                    Source::Analog(subpath) => {
                        let input = read(*subpath);
                        (input.value, input.pressed, input.touched)
                    }
                    Source::Button(subpath) => {
                        let input = read(*subpath);
                        (
                            if input.pressed { 1.0 } else { 0.0 },
                            input.pressed,
                            input.touched,
                        )
                    }
                    Source::Axis {
                        subpath,
                        x,
                        touched,
                    } => {
                        let input = read(*subpath);
                        let value = if !*touched || input.touched {
                            if *x { input.xy.x } else { input.xy.y }
                        } else {
                            0.0
                        };
                        (value, input.pressed, input.touched)
                    }
                };
                let amount = match pressed_override {
                    Some(amount) if pressed => *amount,
                    _ => mapping[0] + value * (mapping[1] - mapping[0]),
                };
                let transform = match shape {
                    Shape::Rotate { pivot, axis } => {
                        Mat4::from_translation(*pivot)
                            * Mat4::from_axis_angle(*axis, amount.to_radians())
                            * Mat4::from_translation(-*pivot)
                    }
                    Shape::Translate { axis } => Mat4::from_translation(*axis * amount),
                };
                (transform, pressed, touched)
            }
            Self::Stick { source, tilt } => {
                let input = read(*source);
                let translate = if input.pressed {
                    tilt.press_translate
                } else {
                    Vec3::ZERO
                };
                (
                    tilt.transform(input.xy, translate),
                    input.pressed,
                    input.touched,
                )
            }
            Self::Pad {
                source,
                tilt,
                touch_translate,
            } => {
                let input = read(*source);
                let mut translate = Vec3::ZERO;
                if input.touched
                    && let Some((x, y)) = touch_translate
                {
                    translate +=
                        Vec3::new(over_axis(*x, input.xy.x), 0.0, over_axis(*y, input.xy.y));
                }
                // The pad only tilts and sinks under a press.
                let (xy, press) = if input.pressed {
                    (input.xy, tilt.press_translate)
                } else {
                    (Vec2::ZERO, Vec3::ZERO)
                };
                (
                    tilt.transform(xy, translate + press),
                    input.pressed,
                    input.touched,
                )
            }
            Self::Dpad { buttons, tilt } => {
                let [up, down, left, right] = buttons.map(read);
                let axis = |positive: &ComponentInput, negative: &ComponentInput| {
                    f32::from(positive.pressed) - f32::from(negative.pressed)
                };
                let xy = Vec2::new(axis(&right, &left), axis(&up, &down));
                let pressed = [&up, &down, &left, &right].iter().any(|b| b.pressed);
                let touched = [&up, &down, &left, &right].iter().any(|b| b.touched);
                let translate = if pressed {
                    tilt.press_translate
                } else {
                    Vec3::ZERO
                };
                (tilt.transform(xy, translate), pressed, touched)
            }
        }
    }
}

impl Component {
    fn from_json(component: ComponentJson) -> Self {
        Self {
            filename: component.filename,
            local: component.component_local.map_or(Mat4::IDENTITY, |local| {
                frame(Some(local.origin), Some(local.rotate_xyz))
            }),
            motion: component.motion.and_then(Motion::from_json),
            visibility: component.visibility.unwrap_or_default(),
        }
    }

    /// The name of the part's mesh file without its extension, which its render model is named
    /// after.
    pub fn file_stem(&self) -> Option<&str> {
        let filename = self.filename.as_deref()?;
        Some(filename.rsplit_once('.').map_or(filename, |(stem, _)| stem))
    }

    /// The part's state for its input, read through `read`, with the app's scroll wheel mode.
    ///
    /// The render model transform is where the part's mesh is drawn, identity at rest. The
    /// local transform is the part's own frame, relative to the mesh: the SteamVR Unity plugin
    /// places the attach point by composing the two, so a pose component like `tip` reports an
    /// identity render model transform and its frame as the local one.
    pub fn state(
        &self,
        read: &dyn Fn(DynSubpath) -> ComponentInput,
        scroll: bool,
    ) -> vr::RenderModel_ComponentState_t {
        let (transform, pressed, touched) = match &self.motion {
            Some(motion) => motion.evaluate(read),
            None => (Mat4::IDENTITY, false, false),
        };
        let mut properties = vr::EVRComponentProperty(0);
        if self.motion.is_none() {
            properties |= vr::EVRComponentProperty::IsStatic;
        }
        if self.visibility.visible(touched, pressed, scroll) {
            properties |= vr::EVRComponentProperty::IsVisible;
        }
        if touched {
            properties |= vr::EVRComponentProperty::IsTouched;
        }
        if pressed {
            properties |= vr::EVRComponentProperty::IsPressed;
        }
        if scroll && self.visibility.scroll == Some(true) {
            properties |= vr::EVRComponentProperty::IsScrolled;
        }
        vr::RenderModel_ComponentState_t {
            mTrackingToComponentRenderModel: super::hmd_matrix(transform),
            mTrackingToComponentLocal: super::hmd_matrix(self.local),
            uProperties: properties.0,
        }
    }

    /// The legacy buttons of the inputs the part moves with.
    pub fn button_mask(&self) -> u64 {
        use vr::EVRButtonId as Id;
        let Some(motion) = &self.motion else {
            return 0;
        };
        motion
            .subpaths()
            .into_iter()
            .flat_map(|subpath| -> &[Id] {
                match subpath {
                    DynSubpath::System => &[Id::System],
                    DynSubpath::Menu | DynSubpath::B | DynSubpath::Y => &[Id::ApplicationMenu],
                    DynSubpath::A | DynSubpath::X => &[Id::A],
                    DynSubpath::Squeeze => &[Id::Grip, Id::Axis2],
                    DynSubpath::Trigger => &[Id::SteamVR_Trigger],
                    DynSubpath::Thumbstick | DynSubpath::Trackpad => &[Id::SteamVR_Touchpad],
                    DynSubpath::DpadUp => &[Id::DPad_Up],
                    DynSubpath::DpadDown => &[Id::DPad_Down],
                    DynSubpath::DpadLeft => &[Id::DPad_Left],
                    DynSubpath::DpadRight => &[Id::DPad_Right],
                    _ => &[],
                }
            })
            .fold(0, |mask, id| mask | 1 << id.0)
    }
}
