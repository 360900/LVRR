mod convert;

pub use bindings::vr::*;
pub use bindings::{VkInstance_T, VkPhysicalDevice_T};
pub use convert::space_relation_to_openvr_pose;
use std::ffi::{CStr, c_void};
use std::sync::{Arc, Weak};

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// Types that are interfaces.
/// # Safety
///
/// Should only be implemented by generated code.
unsafe trait OpenVrInterface: 'static {
    type Vtable: Sync;
}

/// Trait for inheriting from an interface.
/// The thread safety/usage patterns of OpenVR interfaces is not clear, so we err on the safe side and require
/// inheritors to be Sync.
///
/// # Safety
///
/// should not be implemented by hand
#[allow(private_bounds)]
pub unsafe trait Inherits<T: OpenVrInterface>: Sync
where
    Self: Sized,
{
    fn new_wrapped(wrapped: &Arc<Self>) -> VtableWrapper<T, Self>;
    fn init_fntable(init: &Arc<Self>) -> *mut c_void;
}

/// A wrapper around a vtable, to safely pass across FFI.
#[repr(C)]
#[allow(private_bounds)]
pub struct VtableWrapper<T: OpenVrInterface, Wrapped> {
    pub base: T,
    wrapped: Weak<Wrapped>,
}

pub type InterfaceGetter<T> = Box<dyn FnOnce(&Arc<T>) -> *mut c_void>;
pub trait InterfaceImpl: Sync + Send + 'static {
    fn supported_versions() -> &'static [&'static CStr];
    /// Gets a specific interface version
    fn get_version(version: &CStr) -> Option<InterfaceGetter<Self>>;
}

// C sizes on x86_64 and i686 Linux of the structs whose size apps pass in.
const _: () = {
    const fn check<T>(x86_64: usize, i686: usize) {
        let size = if cfg!(target_pointer_width = "64") {
            x86_64
        } else {
            i686
        };
        assert!(size_of::<T>() == size);
    }
    check::<InputAnalogActionData_t>(48, 40);
    check::<InputDigitalActionData_t>(24, 20);
    check::<InputPoseActionData_t>(96, 92);
    check::<InputSkeletalActionData_t>(16, 12);
    check::<vr_1_0_17::InputSkeletalActionData_t>(24, 16);
    check::<InputBindingInfo_t>(544, 544);
    check::<InputOriginInfo_t>(144, 140);
    check::<VRActiveActionSet_t>(32, 32);
    check::<VRControllerState_t>(60, 60);
    check::<VREvent_t>(60, 60);
    check::<Compositor_FrameTiming>(188, 188);
    check::<vr_1_0_3::Compositor_FrameTiming>(172, 172);
    check::<vr_0_9_17::Compositor_FrameTiming>(168, 168);
    check::<vr_0_9_12::Compositor_FrameTiming>(144, 144);
    check::<Compositor_CumulativeStats>(112, 112);
};

#[allow(clippy::derivable_impls)]
impl Default for ETrackingResult {
    fn default() -> Self {
        Self::Uninitialized
    }
}

impl VRTextureBounds_t {
    #[inline]
    pub fn valid(&self) -> bool {
        matches!(
            self,
            VRTextureBounds_t {
                uMin: 0.0..=1.0,
                uMax: 0.0..=1.0,
                vMin: 0.0..=1.0,
                vMax: 0.0..=1.0
            }
        ) && self.vMin != self.vMax
            && self.uMin != self.uMax
    }

    #[inline]
    pub fn vertically_flipped(&self) -> bool {
        self.vMin > self.vMax
    }
}
