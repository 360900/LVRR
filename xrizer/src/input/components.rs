//! Raw controller input for render model components. SteamVR moves the triggers, sticks and
//! buttons of its render models from the driver's raw input, whether or not the app bound them,
//! so these are actions of our own for every input a profile has, in a set synced with whatever
//! the app loaded.

use super::profiles::paths::{DynComponent, DynSubpath};
use super::profiles::{InputDevice, InteractionProfile, LegalPathsT, RunWithProfile};
use glam::Vec2;
use log::debug;
use openxr as xr;
use std::collections::{BTreeSet, HashMap};

/// The inputs SteamVR's render model components read, see `rendermodels::components`.
const SUBPATHS: &[DynSubpath] = &[
    DynSubpath::Trigger,
    DynSubpath::Squeeze,
    DynSubpath::Thumbstick,
    DynSubpath::Trackpad,
    DynSubpath::A,
    DynSubpath::B,
    DynSubpath::X,
    DynSubpath::Y,
    DynSubpath::Menu,
    DynSubpath::System,
    DynSubpath::DpadUp,
    DynSubpath::DpadDown,
    DynSubpath::DpadLeft,
    DynSubpath::DpadRight,
];

enum ComponentAction {
    Bool(xr::Action<bool>),
    Float(xr::Action<f32>),
    Vec2(xr::Action<xr::Vector2f>),
}

pub(super) struct ComponentActionData {
    pub set: xr::ActionSet,
    actions: HashMap<(DynSubpath, Option<DynComponent>), ComponentAction>,
}

/// What a controller input reads, see [ComponentActionData::read].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ComponentInput {
    /// The hand's controller has the input.
    pub active: bool,
    /// The analog value, or 1 for a pressed button. A squeeze reads its force sensor if it has
    /// one, like SteamVR's `/input/grip` on the Index.
    pub value: f32,
    pub xy: Vec2,
    pub pressed: bool,
    pub touched: bool,
}

/// The inputs of every profile that components can read, ordered by name.
fn all_inputs() -> BTreeSet<(usize, Option<DynComponent>)> {
    struct Collect(BTreeSet<(usize, Option<DynComponent>)>);
    impl RunWithProfile for Collect {
        fn run<P: InteractionProfile>(&mut self) {
            let mut paths = Vec::new();
            P::LegalPaths::collect(&mut paths);
            for path in paths {
                if !matches!(path.device, InputDevice::Hand(_)) {
                    continue;
                }
                if let Some(index) = SUBPATHS.iter().position(|s| *s == path.subpath)
                    && !matches!(
                        path.component,
                        Some(DynComponent::Vec2X | DynComponent::Vec2Y)
                    )
                {
                    self.0.insert((index, path.component));
                }
            }
        }
    }
    let mut collect = Collect(BTreeSet::new());
    super::profiles::run_for_all_profiles(&mut collect);
    collect.0
}

impl ComponentActionData {
    pub fn new(instance: &xr::Instance, left_hand: xr::Path, right_hand: xr::Path) -> Self {
        debug!("creating render model component actions");
        let set = instance
            .create_action_set("xrizer-render-models", "XRizer render model components", 0)
            .unwrap();
        let hands = [left_hand, right_hand];
        let actions = all_inputs()
            .into_iter()
            .map(|(index, component)| {
                let subpath = SUBPATHS[index];
                let name = match component {
                    Some(component) => format!("{subpath}-{component}"),
                    None => subpath.to_string(),
                };
                let action = match component {
                    Some(DynComponent::Click | DynComponent::Touch) => {
                        ComponentAction::Bool(set.create_action(&name, &name, &hands).unwrap())
                    }
                    Some(DynComponent::Value | DynComponent::Force) => {
                        ComponentAction::Float(set.create_action(&name, &name, &hands).unwrap())
                    }
                    Some(DynComponent::Vec2X | DynComponent::Vec2Y) => unreachable!(),
                    None => ComponentAction::Vec2(set.create_action(&name, &name, &hands).unwrap()),
                };
                ((subpath, component), action)
            })
            .collect();
        Self { set, actions }
    }

    /// The bindings of `P`'s inputs to these actions, to suggest with the profile's others.
    pub fn bindings<P: InteractionProfile>(&self, instance: &xr::Instance) -> Vec<xr::Binding<'_>> {
        let mut paths = Vec::new();
        P::LegalPaths::collect(&mut paths);
        paths
            .into_iter()
            .filter(|path| matches!(path.device, InputDevice::Hand(_)))
            .filter_map(|path| {
                let action = self.actions.get(&(path.subpath, path.component))?;
                let xr_path = instance.string_to_path(&path.to_string()).ok()?;
                Some(match action {
                    ComponentAction::Bool(action) => xr::Binding::new(action, xr_path),
                    ComponentAction::Float(action) => xr::Binding::new(action, xr_path),
                    ComponentAction::Vec2(action) => xr::Binding::new(action, xr_path),
                })
            })
            .collect()
    }

    /// What `subpath` reads on the hand's controller, after the set was synced.
    pub fn read(
        &self,
        session: &xr::Session<xr::AnyGraphics>,
        hand: xr::Path,
        subpath: DynSubpath,
    ) -> ComponentInput {
        let bool_state = |component| match self.actions.get(&(subpath, Some(component))) {
            Some(ComponentAction::Bool(action)) => action
                .state(session, hand)
                .ok()
                .filter(|state| state.is_active)
                .map(|state| state.current_state),
            _ => None,
        };
        let float_state = |component| match self.actions.get(&(subpath, Some(component))) {
            Some(ComponentAction::Float(action)) => action
                .state(session, hand)
                .ok()
                .filter(|state| state.is_active)
                .map(|state| state.current_state),
            _ => None,
        };
        let xy = match self.actions.get(&(subpath, None)) {
            Some(ComponentAction::Vec2(action)) => action
                .state(session, hand)
                .ok()
                .filter(|state| state.is_active)
                .map(|state| Vec2::new(state.current_state.x, state.current_state.y)),
            _ => None,
        };
        let click = bool_state(DynComponent::Click);
        let touch = bool_state(DynComponent::Touch);
        let value = float_state(DynComponent::Value);
        let force = float_state(DynComponent::Force);

        // The Index trackpad clicks through its force sensor, as in SteamVR's bindings.
        let pressed = click.unwrap_or(false)
            || (subpath == DynSubpath::Trackpad && force.is_some_and(|force| force > 0.5));
        let analog = match subpath {
            DynSubpath::Squeeze => force.or(value),
            _ => value.or(force),
        };
        ComponentInput {
            active: [click, touch].iter().any(Option::is_some)
                || [value, force].iter().any(Option::is_some)
                || xy.is_some(),
            value: analog.unwrap_or(if pressed { 1.0 } else { 0.0 }),
            xy: xy.unwrap_or_default(),
            pressed,
            // A button can't be pressed without being touched.
            touched: touch.unwrap_or(false) || pressed,
        }
    }

    #[cfg(test)]
    pub fn raw_action(
        &self,
        subpath: DynSubpath,
        component: Option<DynComponent>,
    ) -> Option<xr::sys::Action> {
        Some(match self.actions.get(&(subpath, component))? {
            ComponentAction::Bool(action) => action.as_raw(),
            ComponentAction::Float(action) => action.as_raw(),
            ComponentAction::Vec2(action) => action.as_raw(),
        })
    }
}
