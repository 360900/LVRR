mod actions;
mod bindings;
mod context;
mod fallback;

pub(super) use actions::ControllerType;
pub(super) use bindings::{ClickThresholdParams, GrabParameters};

use crate::input::InteractionProfile;
use crate::input::action_manifest::context::BindingsLoadContext;
use crate::input::binding_sources::{BindingChoices, BindingSources};
use crate::input::profiles::{DynInputPath, LegalPathsT, paths::DynSubpath};
use crate::input::steamvr_bindings::Category;
use crate::input::{ActionKey, Input, profiles::RunWithProfile, skeletal::SkeletalInputActionData};
use crate::openxr_data::{self, Hand, SessionData};
use log::{debug, error, info, warn};
use openvr as vr;
use openxr as xr;
use slotmap::{KeyData, SecondaryMap, SlotMap};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
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

        let mut manifest: actions::ActionManifest = serde_json::from_slice(&data).map_err(|e| {
            error!("Failed to parse action manifest: {e}");
            vr::EVRInputError::InvalidParam
        })?;

        let default_bindings = std::mem::take(&mut manifest.default_bindings);
        let loaded = self.create_manifest_actions(
            session_data,
            manifest,
            |context| {
                self.load_bindings(manifest_path.parent().unwrap(), default_bindings, context)
            },
            &[],
        )?;

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

    /// Creates the actions of `manifest` and attaches them, with `extra_sets`, after
    /// `load_bindings` suggested their bindings. It returns the variant of the bindings each
    /// profile got and the files chosen to replace the default bindings.
    fn create_manifest_actions(
        &self,
        session_data: &SessionData,
        manifest: actions::ActionManifest,
        load_bindings: impl FnOnce(
            &mut BindingsLoadContext,
        ) -> (HashMap<xr::Path, String>, BindingChoices),
        extra_sets: &[&xr::ActionSet],
    ) -> Result<super::ManifestLoadedActions, vr::EVRInputError> {
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

        let (binding_variants, binding_choices) = load_bindings(&mut binding_context);

        let context::BindingsLoadContext {
            actions,
            extra_action_sets,
            per_profile_bindings,
            per_profile_pose_bindings,
            per_profile_input_paths,
            per_profile_binding_info,
            head_proximity,
            scalar_constants,
            simulated_properties,
            binding_haptics,
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
            .chain(extra_sets.iter().copied())
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
                    super::ActionData::Pose
                        | super::ActionData::Skeleton(_)
                        | super::ActionData::Vector3 { .. }
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

        let per_profile_binding_info = per_profile_binding_info
            .into_iter()
            .map(|(k, v)| (k, action_map_to_secondary(&mut act_guard, v)))
            .collect();
        let scalar_constants = action_map_to_secondary(&mut act_guard, scalar_constants);

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

        let head_proximity = head_proximity
            .iter()
            .filter_map(|name| {
                let key = act_guard
                    .iter()
                    .find_map(|(key, action)| (action.path == *name).then_some(key))?;
                let set_name = &name[..name.match_indices('/').nth(2)?.0];
                let set = set_guard
                    .iter()
                    .find_map(|(set, name)| (name == set_name).then_some(set))?;
                Some((key, super::HeadProximity::new(set)))
            })
            .collect();

        let binding_haptics = binding_haptics
            .into_iter()
            .filter_map(|(set_name, action, haptic)| {
                let set = set_guard
                    .iter()
                    .find_map(|(set, name)| (*name == set_name).then_some(set))?;
                let action = act_guard
                    .iter()
                    .find_map(|(key, a)| (a.path == action).then_some(key))?;
                Some((set, action, haptic))
            })
            .collect();

        Ok(super::ManifestLoadedActions {
            sets,
            hand_restrictable_sets,
            actions,
            actions_with_custom_bindings,
            per_profile_bindings,
            per_profile_pose_bindings,
            per_profile_input_paths,
            per_profile_binding_info,
            scalar_constants,
            _info_action: info_action,
            info_set,
            haptic_action,
            haptic_set,
            binding_haptics,
            pending_vibrations: Default::default(),
            skeleton_sets,
            head_proximity,
            binding_variants,
            binding_choices,
            simulated_properties,
        })
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

/// The path to bind for `path` on `P`: the path itself, or what the profile translates it to.
fn validate_path<P: InteractionProfile>(path: DynInputPath) -> Option<DynInputPath> {
    let legal = <P::LegalPaths as LegalPathsT>::is_legal;
    // A translation that isn't legal either would fail the whole profile's suggestion.
    legal(path)
        .then_some(path)
        .or_else(|| P::translate_path(path).filter(|path| legal(*path)))
}

/// Whether `P` has an input to bind for `subpath`.
fn has_input<P: InteractionProfile>(hand: Hand, subpath: DynSubpath) -> bool {
    use super::profiles::paths::DynComponent::*;
    [None, Some(Click), Some(Touch), Some(Value), Some(Force)]
        .into_iter()
        .any(|component| {
            validate_path::<P>(DynInputPath {
                hand,
                subpath,
                component,
            })
            .is_some()
        })
}

/// An environment variable, or in tests the value [TEST_ENV] gives it on this thread, since the
/// process environment is shared by the tests running alongside.
pub(super) fn env_var(name: &str) -> Option<OsString> {
    #[cfg(test)]
    if let Some(value) = TEST_ENV.with_borrow(|env| env.get(name).cloned()) {
        return Some(value);
    }
    std::env::var_os(name)
}

#[cfg(test)]
thread_local! {
    pub(super) static TEST_ENV: std::cell::RefCell<HashMap<&'static str, OsString>> =
        Default::default();
}

/// Like [env_var], but tests only see what [TEST_ENV] sets, so the machine's own SteamVR config
/// stays out of them.
#[cfg(not(test))]
pub(super) fn isolated_env_var(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

#[cfg(test)]
pub(super) fn isolated_env_var(name: &str) -> Option<OsString> {
    TEST_ENV.with_borrow(|env| env.get(name).cloned())
}

impl<C: openxr_data::Compositor> Input<C> {
    /// The variant (`options.variant`) of the bindings of `device`'s controller, empty if they
    /// have none. Other handles get the first hand's that has one.
    pub(super) fn binding_variant(&self, device: vr::VRInputValueHandle_t) -> String {
        if self.openxr.is_lost() {
            return String::new();
        }
        let data = self.openxr.session_data.get();
        let Some(loaded) = data.input_data.get_loaded_actions() else {
            return String::new();
        };
        let hand = (device != vr::k_ulInvalidInputValueHandle)
            .then(|| self.hand_from_key(super::InputSourceKey::from(KeyData::from_ffi(device))))
            .flatten();
        let hands = match hand {
            Some(hand) => vec![hand],
            None => vec![Hand::Left, Hand::Right],
        };
        let devices = data.input_data.devices.read().unwrap();
        hands
            .into_iter()
            .find_map(|hand| {
                let profile = devices
                    .get_controller(hand)
                    .map(|device| device.profile_path)
                    .filter(|profile| *profile != xr::Path::NULL)
                    .or_else(|| {
                        data.session
                            .current_interaction_profile(self.get_subaction_path(hand))
                            .ok()
                            .filter(|profile| *profile != xr::Path::NULL)
                    })?;
                loaded.binding_variants.get(&profile).cloned()
            })
            .unwrap_or_default()
    }
}

/// Remembers the variant of the bindings `P` got.
fn record_variant<C: openxr_data::Compositor, P: InteractionProfile>(
    input: &Input<C>,
    bindings: &bindings::Bindings,
    variants: &mut HashMap<xr::Path, String>,
) {
    if let (Some(variant), Ok(profile)) = (
        &bindings.variant,
        input.openxr.instance.string_to_path(P::profile_path()),
    ) {
        variants.insert(profile, variant.clone());
    }
}

/// Whether two paths name the same file, even if spelled differently.
pub(super) fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

impl<C: openxr_data::Compositor> Input<C> {
    /// Suggests the bindings for every profile. Returns the variant of the bindings each profile
    /// got, for GetBindingVariant, and the files chosen to replace the game's.
    fn load_bindings(
        &self,
        parent_path: &Path,
        bindings: Vec<actions::DefaultBindings>,
        context: &mut context::BindingsLoadContext,
    ) -> (HashMap<xr::Path, String>, BindingChoices) {
        // Custom bindings, then the ones chosen in SteamVR, then the game's.
        let sources = BindingSources::new(self.app_key(), Category::SteamVrInput);
        let mut variants = HashMap::new();

        let mut bindings = bindings;
        if let Some(additional_bindings) =
            env_var("XRIZER_ADDITIONAL_BINDINGS").and_then(|value| value.into_string().ok())
        {
            // additional_bindings is a semicolon-separated list of controller_type names.
            bindings.extend(additional_bindings.split(';').map(|s| {
                let controller_type: actions::ControllerType = s.parse().unwrap();
                actions::DefaultBindings {
                    binding_url: PathBuf::from(format!("bindings_{controller_type}.json")),
                    controller_type,
                }
            }));
        }

        let mut loaded = Vec::new();
        for actions::DefaultBindings {
            binding_url,
            controller_type,
        } in bindings
        {
            if let actions::ControllerType::Unknown(other) = &controller_type {
                crate::log_unique!(
                    log::Level::Info,
                    "Ignoring bindings for unknown profile {other}"
                );
                continue;
            }
            // The first file that loads wins, like in SteamVR (e.g. Half-Life: Alyx lists
            // bindings_knuckles.json, then _left and _single variants).
            if loaded.iter().any(|(loaded, _)| *loaded == controller_type) {
                info!("skipping bindings in {binding_url:?}");
                continue;
            }

            // Like SteamVR, the default is used if the chosen bindings don't load.
            let bindings = sources
                .file_for(&controller_type)
                .and_then(|path| read_bindings(&path, &controller_type))
                .or_else(|| read_bindings(&parent_path.join(binding_url), &controller_type));
            let Some(bindings) = bindings else {
                continue;
            };

            if controller_type == actions::ControllerType::GenericHmd {
                if self.openxr.supports_user_presence {
                    bindings::handle_hmd_bindings(context, &bindings);
                } else {
                    // Like SteamVR with a headset that has no proximity sensor.
                    info!(
                        "Ignoring {controller_type} bindings, the runtime doesn't report user presence"
                    );
                }
            } else {
                self.load_controller_bindings(&controller_type, &bindings, context, &mut variants);
            }
            loaded.push((controller_type, bindings));
        }

        // Controllers the game has no bindings for get bindings converted from the closest
        // controller it has them for, so they still work and stay tracked.
        let mut runner = FallbackRunner {
            input: self,
            context,
            loaded: &loaded,
            sources: &sources,
            variants: &mut variants,
        };
        super::profiles::run_for_all_profiles(&mut runner);

        struct FallbackRunner<'a, 'b, 'c, C: openxr_data::Compositor> {
            input: &'a Input<C>,
            context: &'b mut BindingsLoadContext<'c>,
            loaded: &'a [(actions::ControllerType, bindings::Bindings)],
            sources: &'a BindingSources,
            variants: &'a mut HashMap<xr::Path, String>,
        }
        impl<C: openxr_data::Compositor> RunWithProfile for FallbackRunner<'_, '_, '_, C> {
            fn run<P: super::InteractionProfile>(&mut self) {
                let profile = P::profile_path();
                let Ok(profile_path) = self.input.openxr.instance.string_to_path(profile) else {
                    return;
                };
                if self.context.suggested_profiles.contains(&profile_path) {
                    return;
                }
                if !P::has_required_extensions(&self.input.openxr.enabled_extensions) {
                    return;
                }
                // Profiles no SteamVR controller type maps to (hand tracking) have no bindings.
                let Some(native) = actions::ControllerType::known()
                    .into_iter()
                    .find(|ty| ty.covers::<P>())
                else {
                    return;
                };

                let source = native
                    .conversion_sources()
                    .into_iter()
                    .find_map(|ty| self.loaded.iter().find(|(loaded, _)| *loaded == ty));
                // Custom bindings and those chosen in SteamVR may be for controllers the game has
                // none for.
                let own_path = self.sources.file_for(&native);
                let bindings = if let Some((path, own)) = own_path.and_then(|path| {
                    let bindings = read_bindings(&path, &native)?;
                    Some((path, bindings))
                }) {
                    info!("Using {} for {profile}", path.display());
                    own
                } else if let Some((source, bindings)) = source {
                    crate::log_unique!(
                        log::Level::Info,
                        "No {native} bindings for {profile}, converting the {source} bindings"
                    );
                    let mut bindings = bindings.clone();
                    fallback::convert(&mut bindings, source, has_input::<P>);
                    if let Some(properties) = source
                        .simulated_properties()
                        .filter(|_| P::SIMULATE_CONVERTED)
                    {
                        self.context
                            .simulated_properties
                            .insert(profile_path, properties);
                    }
                    bindings
                } else {
                    info!("No bindings to convert for {profile}");
                    bindings::Bindings::default()
                };

                if let Some(mut context) = self.context.for_profile::<C, P>(self.input) {
                    context.adapted = true;
                    self.input
                        .load_bindings_for_profile::<P>(&bindings.bindings, &mut context);
                    record_variant::<C, P>(self.input, &bindings, self.variants);
                }
            }
        }

        (variants, sources.into_choices())
    }

    /// Suggests `bindings` for the profiles of `controller_type`, recording their variant.
    fn load_controller_bindings(
        &self,
        controller_type: &actions::ControllerType,
        bindings: &bindings::Bindings,
        context: &mut BindingsLoadContext,
        variants: &mut HashMap<xr::Path, String>,
    ) {
        // lifetime moment
        struct Runner<'a, 'b, 'c, 'd, C: openxr_data::Compositor>(
            &'a Input<C>,
            &'b mut BindingsLoadContext<'c>,
            &'d bindings::Bindings,
            &'d mut HashMap<xr::Path, String>,
        );
        impl<C: openxr_data::Compositor> RunWithProfile for Runner<'_, '_, '_, '_, C> {
            fn run<P: super::InteractionProfile>(&mut self) {
                if let Some(mut context) = self.1.for_profile::<C, P>(self.0) {
                    // simple_controller gets the Vive's bindings, most of whose inputs it lacks.
                    context.adapted = std::any::TypeId::of::<P>()
                        == std::any::TypeId::of::<
                            super::profiles::simple_controller::SimpleController,
                        >();
                    self.0
                        .load_bindings_for_profile::<P>(&self.2.bindings, &mut context);
                    record_variant::<C, P>(self.0, self.2, self.3);
                } else {
                    warn!(
                        "Couldn't create context for profile {}?",
                        std::any::type_name::<P>()
                    );
                }
            }
        }
        controller_type.run_for_profile(&mut Runner(self, context, bindings, variants));
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
        let path_validator = validate_path::<P>;

        for (action_set_name, bindings) in bindings.iter() {
            let Some(set) = context.get_action_set(action_set_name) else {
                crate::log_unique!(
                    log::Level::Info,
                    "Ignoring the bindings for action set {action_set_name}, which isn't in the action manifest"
                );
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
            bindings::handle_binding_haptics(
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
        bindings::record_binding_info(
            &path_validator,
            P::properties()
                .openvr_controller_type
                .to_str()
                .unwrap_or_default(),
            context,
            bindings,
        );

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
                    Skeleton { .. } | Pose | Vector3 { .. } => unreachable!(),
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
