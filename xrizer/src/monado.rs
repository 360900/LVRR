use libmonado::{Device, DeviceRole, Monado};
use log::{info, warn};

use openvr as vr;

pub struct SafeMonado(pub Monado);

unsafe impl Send for SafeMonado {}
unsafe impl Sync for SafeMonado {}

impl SafeMonado {
    pub fn safe_connect() -> Option<SafeMonado> {
        let monado = match Monado::auto_connect() {
            Ok(m) => m,
            Err(e) => {
                warn!("Failed to connect to Monado: {}", e);
                return None;
            }
        };

        info!("Connected to Monado! Version: {}", monado.get_api_version());
        Some(SafeMonado(monado))
    }

    pub fn get_device_from_vr_index(&self, device_index: u32) -> Option<Device<'_>> {
        match device_index {
            vr::k_unTrackedDeviceIndex_Hmd => self.0.device_from_role(DeviceRole::Head).ok(),
            // Controllers are pre-created at the fixed OpenVR indices 1 (left)
            // and 2 (right); see TrackedDeviceList::new.
            1 => self.0.device_from_role(DeviceRole::Left).ok(),
            2 => self.0.device_from_role(DeviceRole::Right).ok(),
            _ => None,
        }
    }
}
