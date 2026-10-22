mod actions;
mod bindings;
mod context;

pub(super) use actions::ControllerType;
pub(super) use bindings::{ClickThresholdParams, GrabParameters};

use crate::input::InteractionProfile;
use crate::input::action_manifest::context::BindingsLoadContext;
use crate::input::profiles::LegalPathsT;
use crate::input::{ActionKey, Input, profiles::RunWithProfile, skeletal::SkeletalInputActionData};
use crate::openxr_data::{self, Hand, SessionData};
use log::{debug, error, info, warn};
use openvr as vr;
use openxr as xr;
use slotmap::{SecondaryMap, SlotMap};
use std::collections::{HashMap, HashSet};
use std::env::current_dir;
use std::path::{Path, PathBuf};

fn action_map_to_secondary<T>(
    act_guard: &mut SlotMap<ActionKey, super::Action>,
    map: HashMap<String, T>,
) -> SecondaryMap<ActionKey, T> {
    map.into_iter()
        .map(|(name, action)| {
            let key = act_guard
                .iter()
                .find_map(|(key, super::Action { path })| (*path == name).then_some(key))
                .unwrap_or_else(|| act_guard.insert(super::Action { path: name }));

            (key, action)
        })
        .collect()
}

impl<C: openxr_data::Compositor> Input<C> {
    pub(super) fn load_action_manifest(
        &self,
        session_data: &SessionData,
        manifest_path: &Path,
    ) -> Result<(), vr::EVRInputError> {
        match self.loaded_actions_path.get() {
            Some(p) => {
                if !same_file(p, manifest_path) {
                    // SteamVR keeps the first manifest too (e.g. NomaiVR loads a second one
                    // after the game's own).
                    warn!(
                        "Action manifest already loaded from {}, ignoring {}",
                        p.display(),
                        manifest_path.display()
                    );
                    return Err(vr::EVRInputError::MismatchedActionManifest);
                }
                if session_data.input_data.actions.get().is_some() {
                    return Ok(());
                }
            }
            None => {
                if let Some(loaded) = session_data.input_data.actions.get() {
                    error!(
                        "{} actions are already loaded!",
                        if matches!(loaded, super::LoadedActions::Legacy(_)) {
                            "Legacy"
                        } else {
                            "Manifest"
                        }
                    );
                    return Err(vr::EVRInputError::MismatchedActionManifest);
                }
            }
        }

        let data = std::fs::read(manifest_path).map_err(|e| {
            error!("Failed to read manifest {}: {e}", manifest_path.display());
            vr::EVRInputError::InvalidParam
        })?;

        let manifest: actions::ActionManifest = serde_json::from_slice(&data).map_err(|e| {
            error!("Failed to parse action manifest: {e}");
            vr::EVRInputError::InvalidParam
        })?;

        // TODO: support non english localization?
        let english = manifest
            .localization
            .into_iter()
            .find(|l| l.language_tag == "en_US")
            .map(actions::Localization::into_names)
            .unwrap_or_default();

        let mut names = actions::XrNames::default();
        let mut sets = actions::load_action_sets(
            &self.openxr.instance,
            &mut names,
            &english,
            manifest.action_sets,
        );
        debug!("Loaded {} action sets.", sets.len());

        let left_hand_subaction_path = self.get_subaction_path(Hand::Left);
        let right_hand_subaction_path = self.get_subaction_path(Hand::Right);

        let actions = actions::load_actions(
            &self.openxr.instance,
            &mut names,
            &english,
            &mut sets,
            manifest.actions,
            left_hand_subaction_path,
            right_hand_subaction_path,
        );
        debug!("Loaded {} actions.", actions.len());

        let skeletal_input = session_data
            .input_data
            .estimated_skeleton_actions
            .get_or_init(|| {
                SkeletalInputActionData::new(
                    &self.openxr.instance,
                    left_hand_subaction_path,
                    right_hand_subaction_path,
                )
            });

        // See Input::frame_start_update for the explanation of this.
        let info_set = self
            .openxr
            .instance
            .create_action_set("xrizer-info-set", "XRizer info set", 0)
            .unwrap();
        let info_action = info_set
            .create_action::<bool>("xrizer-info-action", "XRizer info action", &[])
            .unwrap();
        // Generate an action set & action for handling haptic pulses.
        // See `System::TriggerHapticPulse` & `Input::legacy_haptic`.
        let haptic_set = self
            .openxr
            .instance
            .create_action_set("xrizer-haptic-set", "XRizer haptic set", 0)
            .unwrap();
        let haptic_action = haptic_set
            .create_action::<xr::Haptic>(
                "xrizer-haptic-action",
                "XRizer haptic action",
                &[self.subaction_paths.left, self.subaction_paths.right],
            )
            .unwrap();

        let mut binding_context = context::BindingsLoadContext::new(
            &sets,
            actions,
            names,
            &session_data.input_data.pose_data.grip,
            &info_action,
            &haptic_action,
            skeletal_input,
        );

        self.load_bindings(
            manifest_path.parent().unwrap(),
            manifest.default_bindings,
            &mut binding_context,
        );

        let context::BindingsLoadContext {
            actions,
            extra_action_sets,
            per_profile_bindings,
            per_profile_pose_bindings,
            per_profile_input_paths,
            ..
        } = binding_context;

        let xr_sets: Vec<_> = sets
            .values()
            .chain([
                &session_data.input_data.pose_data.set,
                &info_set,
                &haptic_set,
                &skeletal_input.set,
            ])
            .collect();
        let attached = session_data.session.attach_action_sets(&xr_sets);
        if self
            .openxr
            .check("xrAttachSessionActionSets", attached)
            .is_none()
        {
            return Err(vr::EVRInputError::IPCError);
        }

        // Try forcing an interaction profile now
        let synced = session_data
            .session
            .sync_actions(&[xr::ActiveActionSet::new(&info_set)]);
        self.openxr.check("xrSyncActions", synced);

        // Every manifest OpenXR action is created with both hand subaction paths, so a set can
        // only be synced restricted to a hand if it has at least one.
        let sets_with_xr_actions: HashSet<String> = actions
            .iter()
            .filter(|(_, data)| {
                !matches!(
                    data,
                    super::ActionData::Pose | super::ActionData::Skeleton(_)
                )
            })
            .filter_map(|(path, _)| path.match_indices('/').nth(2).map(|(idx, _)| &path[..idx]))
            .map(str::to_string)
            .chain(extra_action_sets)
            .collect();
        let mut hand_restrictable_sets = HashSet::new();

        // Transform actions and sets into maps
        // If the application has already requested the handle for an action/set, we need to
        // reuse the corresponding slot. Otherwise just create a new one.
        let mut set_guard = self.set_map.write().unwrap();
        let sets: SecondaryMap<_, _> = sets
            .into_iter()
            .map(|(set_name, set)| {
                // This function is only called when loading the action manifest, and most games
                // don't have a ton of actions, so a linear search through the map is probably fine.
                let key = set_guard
                    .iter()
                    .find_map(|(key, set_path)| (*set_path == set_name).then_some(key))
                    .unwrap_or_else(|| set_guard.insert(set_name.clone()));
                if sets_with_xr_actions.contains(&set_name) {
                    hand_restrictable_sets.insert(key);
                }
                (key, set)
            })
            .collect();

        let mut act_guard = self.action_map.write().unwrap();
        let actions = action_map_to_secondary(&mut act_guard, actions);

        let mut actions_with_custom_bindings = HashSet::new();
        let per_profile_bindings = per_profile_bindings
            .into_iter()
            .map(|(k, v)| {
                (k, {
                    v.into_iter()
                        .map(|(name, actions)| {
                            let key = act_guard
                                .iter()
                                .find_map(|(key, super::Action { path })| {
                                    (*path == name).then_some(key)
                                })
                                .unwrap_or_else(|| {
                                    act_guard.insert(super::Action { path: name.clone() })
                                });

                            actions_with_custom_bindings.insert(key);

                            (key, actions)
                        })
                        .collect()
                })
            })
            .collect();

        let per_profile_pose_bindings = per_profile_pose_bindings
            .into_iter()
            .map(|(k, v)| (k, action_map_to_secondary(&mut act_guard, v)))
            .collect();

        let per_profile_input_paths = per_profile_input_paths
            .into_iter()
            .map(|(k, v)| (k, action_map_to_secondary(&mut act_guard, v)))
            .collect();

        // An action belongs to the set its path starts with, e.g. /actions/main/in/hand_left.
        let skeleton_sets = actions
            .iter()
            .filter(|(_, data)| matches!(data, super::ActionData::Skeleton(_)))
            .filter_map(|(key, _)| {
                let path = &act_guard[key].path;
                let set_name = &path[..path.match_indices('/').nth(2)?.0];
                let set = set_guard
                    .iter()
                    .find_map(|(set, name)| (name == set_name).then_some(set))?;
                Some((key, set))
            })
            .collect();

        let loaded = super::ManifestLoadedActions {
            sets,
            hand_restrictable_sets,
            actions,
            actions_with_custom_bindings,
            per_profile_bindings,
            per_profile_pose_bindings,
            per_profile_input_paths,
            _info_action: info_action,
            info_set,
            haptic_action,
            haptic_set,
            skeleton_sets,
        };

        session_data
            .input_data
            .actions
            .set(super::LoadedActions::Manifest(Box::new(loaded)))
            .unwrap_or_else(|_| unreachable!());
        // Only remember the path once it loaded, so a failed attempt doesn't block a retry with
        // the right path.
        let _ = self.loaded_actions_path.set(manifest_path.to_path_buf());
        Ok(())
    }
}

fn read_bindings(
    path: &Path,
    controller_type: &actions::ControllerType,
) -> Option<bindings::Bindings> {
    debug!(
        "Reading bindings for {controller_type} (at {})",
        path.display()
    );
    let data = std::fs::read(path)
        .inspect_err(|e| error!("Couldn't load bindings for {controller_type}: {e}"))
        .ok()?;
    serde_json::from_slice(&data)
        .inspect_err(|e| error!("Failed to parse bindings for {controller_type}: {e}"))
        .ok()
}

/// Whether two paths name the same file, even if spelled differently.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

impl<C: openxr_data::Compositor> Input<C> {
    fn load_bindings(
        &self,
        parent_path: &Path,
        bindings: Vec<actions::DefaultBindings>,
        context: &mut context::BindingsLoadContext,
    ) {
        let custom_bindings_dir = std::env::var_os("XRIZER_CUSTOM_BINDINGS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| current_dir().unwrap_or_default().join("xrizer"));

        let mut bindings = bindings;
        if let Ok(additional_bindings) = std::env::var("XRIZER_ADDITIONAL_BINDINGS") {
            // additional_bindings is a semicolon-separated list of controller_type names.
            bindings.extend(additional_bindings.split(';').map(|s| {
                let controller_type: actions::ControllerType = s.parse().unwrap();
                actions::DefaultBindings {
                    binding_url: PathBuf::from(format!("bindings_{controller_type}.json")),
                    controller_type,
                }
            }));
        }

        let mut loaded_types = HashSet::new();
        for actions::DefaultBindings {
            binding_url,
            controller_type,
        } in bindings
        {
            if let actions::ControllerType::Unknown(other) = &controller_type {
                info!("Ignoring bindings for unknown profile {other}");
                continue;
            }
            // The first file that loads wins, like in SteamVR (e.g. Half-Life: Alyx lists
            // bindings_knuckles.json, then _left and _single variants).
            if loaded_types.contains(&controller_type) {
                info!("skipping bindings in {binding_url:?}");
                continue;
            }

            let custom_path = custom_bindings_dir.join(format!("{controller_type}.json"));
            let bindings_path = match custom_path.exists() {
                true => custom_path,
                false => parent_path.join(binding_url),
            };
            let Some(bindings) = read_bindings(&bindings_path, &controller_type) else {
                continue;
            };
            loaded_types.insert(controller_type.clone());

            let mut runner = Runner(self, context, bindings);
            controller_type.run_for_profile(&mut runner);

            // lifetime moment
            struct Runner<'a, 'b, 'c, C: openxr_data::Compositor>(
                &'a Input<C>,
                &'b mut BindingsLoadContext<'c>,
                bindings::Bindings,
            );
            impl<C: openxr_data::Compositor> RunWithProfile for Runner<'_, '_, '_, C> {
                fn run<P: super::InteractionProfile>(&mut self) {
                    if let Some(mut context) = self.1.for_profile::<C, P>(self.0) {
                        self.0
                            .load_bindings_for_profile::<P>(&self.2.bindings, &mut context);
                    } else {
                        warn!(
                            "Couldn't create context for profile {}?",
                            std::any::type_name::<P>()
                        );
                    }
                }
            }
        }
    }

    fn load_bindings_for_profile<P: InteractionProfile>(
        &self,
        bindings: &HashMap<String, bindings::ActionSetBinding>,
        context: &mut context::BindingsProfileLoadContext<'_>,
    ) {
        info!("loading bindings for {}", P::profile_path());

        let conv = super::profiles::InputToXrPath::new(&self.openxr.instance);
        let legacy_bindings = P::legacy_bindings(&conv);
        let skeletal_bindings = P::skeletal_input_bindings(&conv);
        let profile_path = self
            .openxr
            .instance
            .string_to_path(P::profile_path())
            .unwrap();
        let path_validator = |path| {
            <P::LegalPaths as LegalPathsT>::is_legal(path)
                .then_some(path)
                .or_else(|| P::translate_path(path))
        };

        for (action_set_name, bindings) in bindings.iter() {
            let Some(set) = context.get_action_set(action_set_name) else {
                warn!("Action set {action_set_name} missing.");
                continue;
            };

            let set = set.clone();

            bindings::handle_haptic_bindings(&self.openxr.instance, context, &bindings.haptics);
            bindings::handle_pose_bindings(context, &bindings.poses);
            bindings::handle_skeleton_bindings(context, &bindings.skeleton);

            bindings::handle_sources(
                &path_validator,
                context,
                action_set_name,
                &set,
                &bindings.sources,
            );
            bindings::handle_chord_bindings(
                &path_validator,
                context,
                action_set_name,
                &set,
                &bindings.chords,
            );
        }

        let info_action_binding = *legacy_bindings
            .trigger_click
            .first()
            .unwrap_or_else(|| panic!("Missing trigger_click binding for {}", P::profile_path()));
        // Custom bindings on the same input share an action.
        let mut seen = HashSet::new();
        let bindings: Vec<xr::Binding<'_>> = context
            .bindings
            .iter()
            .filter(|binding| seen.insert(*binding))
            .map(|(name, path)| {
                use super::ActionData::*;
                let path = *path;
                match context
                    .actions
                    .get(name)
                    .unwrap_or_else(|| panic!("Couldn't find data for action {name}"))
                {
                    Bool(action) => xr::Binding::new(action, path),
                    Vector1 { action, .. } => xr::Binding::new(action, path),
                    Vector2 { action, .. } => xr::Binding::new(action, path),
                    Haptic(action) => xr::Binding::new(action, path),
                    Skeleton { .. } | Pose => unreachable!(),
                }
            })
            .chain(
                legacy_bindings
                    .extra
                    .grip_pose
                    .into_iter()
                    .map(|path| xr::Binding::new(context.grip_action, path)),
            )
            .chain(std::iter::once(xr::Binding::new(
                context.info_action,
                info_action_binding,
            )))
            .chain(
                legacy_bindings
                    .haptic
                    .into_iter()
                    .map(|path| xr::Binding::new(context.haptic_action, path)),
            )
            .chain(skeletal_bindings.binding_iter(&context.skeletal_input.actions))
            .collect();

        match self
            .openxr
            .instance
            .suggest_interaction_profile_bindings(profile_path, &bindings)
        {
            Ok(()) => debug!(
                "suggested {} bindings for {}",
                bindings.len(),
                P::profile_path()
            ),
            Err(e) => warn!(
                "Couldn't suggest bindings for {}, skipping it: {e}",
                P::profile_path()
            ),
        }
    }
}
