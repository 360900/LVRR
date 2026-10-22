use std::ffi::CStr;

use crate::input::legacy::button_mask_from_id;
use crate::{button_mask_from_ids, input::profiles::Property};
use openvr::EVRButtonId as btn;

pub mod hp_motion_controller;
pub mod ms_motion_controller;
pub mod samsung_odyssey_controller;

const TRACKING_SYSTEM_NAME: &CStr = c"holographic";
const SERIAL_NUMBER: Property<&CStr> = Property::PerHand {
    left: c"MRSOURCE0",
    right: c"MRSOURCE1",
};
const OG_OPENVR_CONTROLLER_TYPE: &CStr = c"holographic_controller";
const OG_LEGACY_BUTTONS_MASK: u64 = button_mask_from_ids!(
    btn::System,
    btn::ApplicationMenu,
    btn::Grip,
    btn::Axis0,
    btn::Axis1,
    btn::Axis2,
    btn::A
);
