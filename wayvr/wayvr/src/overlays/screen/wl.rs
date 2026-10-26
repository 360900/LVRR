use glam::{DVec2, vec2};
use wgui::log::LogErr;
use wlx_capture::{
    ext_image_copy_capture::ExtImageCopyCapture,
    frame::Transform,
    kde_screencast::KdeScreencastCapture,
    mutter_screencast::MutterScreencastCapture,
    wayland::{WlxClient, WlxOutput},
    wlr_screencopy::WlrScreencopyCapture,
};
use wlx_common::{astr_containers::AStrMapExt, config::CaptureMethod};

use crate::{
    overlays::screen::{backend::CaptureType, create_screen_from_backend, pw::ScreenCastBackend},
    state::{AppState, ScreenMeta},
    windowing::backend::OverlayBackend,
};

use super::{ScreenCreateData, backend::ScreenBackend, capture::new_wlx_capture};

impl ScreenBackend {
    pub fn new_wlr_screencopy(output: &WlxOutput, app: &AppState) -> Option<Self> {
        let client = WlxClient::new()?;
        let capture = new_wlx_capture!(app, WlrScreencopyCapture::new(client, output.id));
        Some(Self::new_raw(
            output.name.clone(),
            app.feats.xr_backend,
            CaptureType::ScreenCopy,
            capture,
        ))
    }

    /// LVRR: ext-image-copy-capture, wlr-screencopy's successor on wlroots, Hyprland, niri, COSMIC
    pub fn new_ext_image_copy(output: &WlxOutput, app: &AppState) -> Option<Self> {
        let client = Box::new(WlxClient::new()?);
        let capture = new_wlx_capture!(app, ExtImageCopyCapture::new(client, output.id)?);
        Some(Self::new_raw(
            output.name.clone(),
            app.feats.xr_backend,
            CaptureType::ScreenCopy,
            capture,
        ))
    }

    /// LVRR: GNOME's compositor records the monitor over D-Bus, without the portal's picker
    pub fn new_mutter_screencast(output: &WlxOutput, app: &AppState) -> Option<Self> {
        let capture = new_wlx_capture!(
            app,
            MutterScreencastCapture::new(output.name.clone(), &output.name, true)?
        );
        Some(Self::new_raw(
            output.name.clone(),
            app.feats.xr_backend,
            CaptureType::PipeWire,
            capture,
        ))
    }

    /// LVRR: KWin's own screencast, a PipeWire stream of the output without the portal's picker
    pub fn new_kde_screencast(output: &WlxOutput, app: &AppState) -> Option<Self> {
        let client = Box::new(WlxClient::new()?);
        // the macro names the capture in each of its branches, the frame type differs
        let capture = new_wlx_capture!(app, KdeScreencastCapture::new(client, output.id, true)?);
        Some(Self::new_raw(
            output.name.clone(),
            app.feats.xr_backend,
            CaptureType::PipeWire,
            capture,
        ))
    }
}

pub struct WlCaptureProtocols {
    pub wlr_screencopy: bool,
    pub ext_image_copy: bool,
    pub kde_screencast: bool,
}

pub fn create_screen_renderer_wl(
    output: &WlxOutput,
    protocols: &WlCaptureProtocols,
    app: &mut AppState,
) -> anyhow::Result<Box<dyn OverlayBackend>> {
    if matches!(
        app.session.config.capture_method,
        CaptureMethod::ScreenCopyCpu | CaptureMethod::ScreenCopyGpu | CaptureMethod::Auto
    ) && protocols.wlr_screencopy
        && let Some(mut backend) = ScreenBackend::new_wlr_screencopy(output, app)
    {
        log::info!("{}: Using ScreenCopy capture", output.name);
        backend.logical_pos = vec2(output.logical_pos.0 as f32, output.logical_pos.1 as f32);
        backend.logical_size = vec2(output.logical_size.0 as f32, output.logical_size.1 as f32);
        backend.apply_mouse_transform_with_override(Transform::Undefined);
        return Ok(Box::new(backend));
    }

    if matches!(
        app.session.config.capture_method,
        CaptureMethod::ScreenCopyCpu | CaptureMethod::ScreenCopyGpu | CaptureMethod::Auto
    ) && protocols.ext_image_copy
        && let Some(mut backend) = ScreenBackend::new_ext_image_copy(output, app)
    {
        log::info!("{}: Using image copy capture", output.name);
        backend.logical_pos = vec2(output.logical_pos.0 as f32, output.logical_pos.1 as f32);
        backend.logical_size = vec2(output.logical_size.0 as f32, output.logical_size.1 as f32);
        backend.apply_mouse_transform_with_override(Transform::Undefined);
        return Ok(Box::new(backend));
    }

    if matches!(
        app.session.config.capture_method,
        CaptureMethod::PipeWire | CaptureMethod::Auto
    ) && protocols.kde_screencast
        && let Some(mut backend) = ScreenBackend::new_kde_screencast(output, app)
    {
        log::info!("{}: Using KDE screencast capture", output.name);
        backend.logical_pos = vec2(output.logical_pos.0 as f32, output.logical_pos.1 as f32);
        backend.logical_size = vec2(output.logical_size.0 as f32, output.logical_size.1 as f32);
        backend.apply_mouse_transform_with_override(Transform::Undefined);
        return Ok(Box::new(backend));
    }

    if matches!(
        app.session.config.capture_method,
        CaptureMethod::PipeWire | CaptureMethod::Auto
    ) && let Some(mut backend) = ScreenBackend::new_mutter_screencast(output, app)
    {
        log::info!("{}: Using Mutter screencast capture", output.name);
        backend.logical_pos = vec2(output.logical_pos.0 as f32, output.logical_pos.1 as f32);
        backend.logical_size = vec2(output.logical_size.0 as f32, output.logical_size.1 as f32);
        backend.apply_mouse_transform_with_override(Transform::Undefined);
        return Ok(Box::new(backend));
    }

    log::info!("{}: Using Pipewire capture", output.name);
    let display_name = &*output.name;

    // Find existing token by display
    let token = app
        .session
        .pw_tokens
        .arc_get(display_name)
        .map(|x| x.clone().into());

    if token.is_some() {
        log::info!("Found existing Pipewire token for display {display_name}");
    }

    Ok(Box::new(
        ScreenCastBackend::new_wl(output, token, app)
            .log_err("Failed to create screen with screen cast backend")?,
    ))
}

pub fn create_screens_wayland(
    wl: &mut WlxClient,
    app: &mut AppState,
) -> anyhow::Result<ScreenCreateData> {
    let mut screens = vec![];
    let protocols = WlCaptureProtocols {
        wlr_screencopy: wl.maybe_wlr_screencopy_mgr.is_some(),
        ext_image_copy: wl.maybe_ext_image_copy_capture_mgr.is_some()
            && wl.maybe_ext_output_source_mgr.is_some(),
        kde_screencast: wl.maybe_kde_screencast_mgr.is_some(),
    };

    for (id, output) in &wl.outputs {
        if app.screens.iter().any(|s| s.name == output.name) {
            continue;
        }

        log::info!(
            "{}: Init screen of res {:?}, logical {:?} at {:?}",
            output.name,
            output.size,
            output.logical_size,
            output.logical_pos,
        );

        let backend = create_screen_renderer_wl(output, &protocols, app)?;
        let window_config = create_screen_from_backend(
            output.name.clone(),
            output.transform,
            &app.session,
            backend,
        );

        let meta = ScreenMeta {
            name: wl.outputs[id].name.clone(),
            native_handle: *id,
        };

        screens.push((meta, window_config));
    }

    let extent = wl.get_desktop_extent();
    let origin = wl.get_desktop_origin();

    app.hid_provider
        .inner
        .set_desktop_extent(DVec2::new(extent.0 as _, extent.1 as _));
    app.hid_provider
        .inner
        .set_desktop_origin(DVec2::new(origin.0 as _, origin.1 as _));

    Ok(ScreenCreateData { screens })
}
