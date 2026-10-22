//! XR_EXT_hand_tracking and XR_EXT_hand_tracking_data_source, with all joints at the origin.

use super::{Handle, UserPath, destroy_handle, get_handle, impl_handle};
use openxr_sys as xr;
use openxr_sys::Handle as _;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

thread_local! {
    static SUPPORTED: Cell<bool> = const { Cell::new(false) };
    static CREATED: Cell<usize> = const { Cell::new(0) };
    static LOCATED: Cell<usize> = const { Cell::new(0) };
}

/// Active hands and their data source, by session and hand.
static TRACKED: Mutex<Option<HashMap<(u64, usize), xr::HandTrackingDataSourceEXT>>> =
    Mutex::new(None);

/// Sets whether systems queried and hand trackers created on this thread support hand tracking.
pub fn set_hand_tracking_supported(supported: bool) {
    SUPPORTED.set(supported);
}

/// How many hand trackers were created on this thread, successfully or not.
pub fn hand_trackers_created() -> usize {
    CREATED.get()
}

/// How many times hand joints were located on this thread.
pub fn hand_joints_located() -> usize {
    LOCATED.get()
}

/// Makes the hand tracked from `source`, or untracked if `None`.
pub fn set_hand_tracked(
    session: xr::Session,
    hand: UserPath,
    source: Option<xr::HandTrackingDataSourceEXT>,
) {
    let mut tracked = TRACKED.lock().unwrap();
    let tracked = tracked.get_or_insert_default();
    let key = (session.into_raw(), hand as usize);
    match source {
        Some(source) => tracked.insert(key, source),
        None => tracked.remove(&key),
    };
}

pub(super) fn fill_system_properties(mut next: *mut xr::BaseOutStructure) {
    while !next.is_null() {
        let xr::BaseOutStructure {
            ty,
            next: following,
        } = unsafe { *next };
        if ty == xr::SystemHandTrackingPropertiesEXT::TYPE {
            let props = unsafe { &mut *(next as *mut xr::SystemHandTrackingPropertiesEXT) };
            props.supports_hand_tracking = SUPPORTED.get().into();
        }
        next = following;
    }
}

pub(super) struct HandTracker {
    session: xr::Session,
    hand: UserPath,
    requested_sources: bool,
}

impl_handle!(HandTracker, xr::HandTrackerEXT);

pub(super) extern "system" fn create_hand_tracker_e_x_t(
    session: xr::Session,
    info: *const xr::HandTrackerCreateInfoEXT,
    tracker: *mut xr::HandTrackerEXT,
) -> xr::Result {
    get_handle!(session);
    CREATED.set(CREATED.get() + 1);
    if !SUPPORTED.get() {
        return xr::Result::ERROR_FEATURE_UNSUPPORTED;
    }

    let info = unsafe { info.as_ref().unwrap() };
    let mut requested_sources = false;
    let mut next = info.next as *const xr::BaseInStructure;
    while !next.is_null() {
        let xr::BaseInStructure {
            ty,
            next: following,
        } = unsafe { *next };
        if ty == xr::HandTrackingDataSourceInfoEXT::TYPE {
            let sources = unsafe { &*(next as *const xr::HandTrackingDataSourceInfoEXT) };
            requested_sources = sources.requested_data_source_count > 0;
        }
        next = following;
    }

    let hand = match info.hand {
        xr::HandEXT::LEFT => UserPath::LeftHand,
        xr::HandEXT::RIGHT => UserPath::RightHand,
        _ => return xr::Result::ERROR_VALIDATION_FAILURE,
    };
    let created = Arc::new(HandTracker {
        session,
        hand,
        requested_sources,
    });
    unsafe { *tracker = created.to_xr() };
    xr::Result::SUCCESS
}

pub(super) extern "system" fn destroy_hand_tracker_e_x_t(
    tracker: xr::HandTrackerEXT,
) -> xr::Result {
    destroy_handle(tracker)
}

pub(super) extern "system" fn locate_hand_joints_e_x_t(
    tracker: xr::HandTrackerEXT,
    _info: *const xr::HandJointsLocateInfoEXT,
    locations: *mut xr::HandJointLocationsEXT,
) -> xr::Result {
    let tracker = get_handle!(tracker);
    LOCATED.set(LOCATED.get() + 1);
    let locations = unsafe { locations.as_mut().unwrap() };
    let source = TRACKED.lock().unwrap().as_ref().and_then(|t| {
        t.get(&(tracker.session.into_raw(), tracker.hand as usize))
            .copied()
    });

    let mut next = locations.next as *mut xr::BaseOutStructure;
    while !next.is_null() {
        let xr::BaseOutStructure {
            ty,
            next: following,
        } = unsafe { *next };
        if ty == xr::HandTrackingDataSourceStateEXT::TYPE {
            // Like Monado: the state can only be queried for trackers that requested sources.
            if !tracker.requested_sources {
                return xr::Result::ERROR_VALIDATION_FAILURE;
            }
            let state = unsafe { &mut *(next as *mut xr::HandTrackingDataSourceStateEXT) };
            state.is_active = source.is_some().into();
            state.data_source = source.unwrap_or(xr::HandTrackingDataSourceEXT::UNOBSTRUCTED);
        } else if ty == xr::HandJointVelocitiesEXT::TYPE {
            let velocities = unsafe { &mut *(next as *mut xr::HandJointVelocitiesEXT) };
            for i in 0..velocities.joint_count as usize {
                unsafe { velocities.joint_velocities.add(i).write(Default::default()) };
            }
        }
        next = following;
    }

    locations.is_active = source.is_some().into();
    let flags = if source.is_some() {
        xr::SpaceLocationFlags::POSITION_VALID
            | xr::SpaceLocationFlags::POSITION_TRACKED
            | xr::SpaceLocationFlags::ORIENTATION_VALID
            | xr::SpaceLocationFlags::ORIENTATION_TRACKED
    } else {
        xr::SpaceLocationFlags::EMPTY
    };
    // The output arrays may be uninitialized.
    for i in 0..locations.joint_count as usize {
        unsafe {
            locations
                .joint_locations
                .add(i)
                .write(xr::HandJointLocationEXT {
                    location_flags: flags,
                    pose: xr::Posef::IDENTITY,
                    radius: 0.01,
                })
        };
    }
    xr::Result::SUCCESS
}
