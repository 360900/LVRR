use openvr as vr;
use std::sync::Mutex;

/// Settings for the desktop headset mirror. We have no mirror window, so these are only stored
/// and returned back to the app.
#[derive(macros::InterfaceImpl)]
#[interface = "IVRHeadsetView"]
#[versions(001)]
pub struct HeadsetView {
    vtables: Vtables,
    state: Mutex<HeadsetViewState>,
}

struct HeadsetViewState {
    size: (u32, u32),
    mode: vr::HeadsetViewMode_t,
    cropped: bool,
    blend_range: (f32, f32),
}

impl Default for HeadsetView {
    fn default() -> Self {
        Self {
            vtables: Default::default(),
            state: Mutex::new(HeadsetViewState {
                size: (0, 0),
                mode: vr::HeadsetViewMode_t::Both,
                cropped: false,
                blend_range: (0.0, 0.0),
            }),
        }
    }
}

impl vr::IVRHeadsetView001_Interface for HeadsetView {
    fn SetHeadsetViewSize(&self, width: u32, height: u32) {
        self.state.lock().unwrap().size = (width, height);
    }

    fn GetHeadsetViewSize(&self, width: *mut u32, height: *mut u32) {
        let (w, h) = self.state.lock().unwrap().size;
        if let Some(width) = unsafe { width.as_mut() } {
            *width = w;
        }
        if let Some(height) = unsafe { height.as_mut() } {
            *height = h;
        }
    }

    fn SetHeadsetViewMode(&self, mode: vr::HeadsetViewMode_t) {
        self.state.lock().unwrap().mode = mode;
    }

    fn GetHeadsetViewMode(&self) -> vr::HeadsetViewMode_t {
        self.state.lock().unwrap().mode
    }

    fn SetHeadsetViewCropped(&self, cropped: bool) {
        self.state.lock().unwrap().cropped = cropped;
    }

    fn GetHeadsetViewCropped(&self) -> bool {
        self.state.lock().unwrap().cropped
    }

    fn GetHeadsetViewAspectRatio(&self) -> f32 {
        let (w, h) = self.state.lock().unwrap().size;
        if h == 0 { 0.0 } else { w as f32 / h as f32 }
    }

    fn SetHeadsetViewBlendRange(&self, start: f32, end: f32) {
        self.state.lock().unwrap().blend_range = (start, end);
    }

    fn GetHeadsetViewBlendRange(&self, start: *mut f32, end: *mut f32) {
        let (s, e) = self.state.lock().unwrap().blend_range;
        if let Some(start) = unsafe { start.as_mut() } {
            *start = s;
        }
        if let Some(end) = unsafe { end.as_mut() } {
            *end = e;
        }
    }
}
