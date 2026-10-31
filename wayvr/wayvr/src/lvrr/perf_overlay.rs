//! LVRR: a small head-locked panel with streaming statistics.

use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use anyhow::Context;
use glam::{Affine3A, Quat, Vec3, vec3};
use lvrr_common::{
    headset::{DecoderTiming, HeadsetStats},
    state::LvrrBackend,
};
use wgui::{
    event::EventListenerKind,
    i18n::Translation,
    parser::Fetchable,
    widget::{EventResult, label::WidgetLabel},
};
use wlx_common::windowing::{OverlayWindowState, Positioning};

use crate::{
    backend::task::{OverlayTask, SpawnPos, TaskType},
    gui::{
        panel::{GuiPanel, NewGuiPanelParams},
        timer::GuiTimer,
    },
    state::AppState,
    windowing::{OverlaySelector, PIXELS_TO_METERS, Z_ORDER_HELP, window::OverlayWindowConfig},
};

static NAME: LazyLock<Arc<str>> = LazyLock::new(|| "lvrr-perf".into());

/// The server sends statistics once a second
const REFRESH: Duration = Duration::from_millis(500);

type Field = fn(&HeadsetStats) -> String;

/// Label ids in perf_overlay.xml and their values, as Home and PC > Performance show them
const FIELDS: [(&str, Field); 12] = [
    ("refresh_rate", |s| format!("{:.0} Hz", s.display_hz)),
    ("game_fps", |s| {
        s.game_fps
            .map_or_else(|| "-".to_string(), |fps| format!("{fps:.0} fps"))
    }),
    ("game_missed", |s| format!("{:.0}/s", s.game_missed_fps)),
    ("encoder_skipped", |s| {
        format!("{:.0}/s", s.encoder_skipped_fps)
    }),
    ("latency", |s| format!("{:.0} ms", s.latency_ms())),
    ("encode", |s| stage(s, |d| d.encode_ms)),
    ("send", |s| stage(s, |d| d.send_ms)),
    ("transfer", |s| stage(s, |d| d.network_ms)),
    ("decode", |s| stage(s, |d| d.decode_ms)),
    ("display", |s| stage(s, |d| d.display_ms)),
    ("network", |s| format!("{:.0} Mbit/s", s.rx_bps / 1e6)),
    ("headset_gpu", |s| format!("{:.1} ms", s.gpu_time_ms)),
];

/// A latency stage of the slowest video stream, the one `latency` shows
fn stage(stats: &HeadsetStats, value: fn(&DecoderTiming) -> f32) -> String {
    stats
        .slowest_decoder()
        .map_or_else(|| "-".to_string(), |d| format!("{:.1} ms", value(d)))
}

pub fn set_visible(app: &mut AppState, visible: bool) {
    let selector = OverlaySelector::Name(NAME.clone());
    let task = if visible {
        OverlayTask::Spawn(selector, SpawnPos::Fixed, Box::new(new_perf_overlay))
    } else {
        OverlayTask::Drop(selector)
    };
    app.tasks.enqueue(TaskType::Overlay(task));
}

fn new_perf_overlay(app: &mut AppState) -> Option<OverlayWindowConfig> {
    new_panel(app)
        .inspect_err(|e| log::error!("Could not create the performance overlay: {e:?}"))
        .ok()
}

fn new_panel(app: &mut AppState) -> anyhow::Result<OverlayWindowConfig> {
    let mut panel = GuiPanel::new_from_template(
        app,
        "gui/lvrr/perf_overlay.xml",
        (),
        NewGuiPanelParams::default(),
    )?;

    for (id, field) in FIELDS {
        let widget = panel
            .parser_state
            .get_widget_id(id)
            .with_context(|| format!("perf_overlay.xml: missing element with id: {id}"))?;
        panel.layout.add_event_listener::<AppState, ()>(
            widget,
            EventListenerKind::InternalStateChange,
            Box::new(move |common, data, app, ()| {
                let text = app
                    .lvrr
                    .link
                    .as_ref()
                    .and_then(|link| link.state().latest_stats())
                    .map_or_else(|| "-".to_string(), field);
                if let Some(label) = data.obj.get_as_mut::<WidgetLabel>() {
                    label.set_text(common, Translation::from_raw_text_string(text));
                }
                Ok(EventResult::Pass)
            }),
        );
    }
    panel.timers.push(GuiTimer::new(REFRESH, 0));
    panel.update_layout(app)?;

    Ok(OverlayWindowConfig {
        name: NAME.clone(),
        default_state: OverlayWindowState {
            // upper left of the view
            positioning: Positioning::FollowHead { lerp: 1.0 },
            transform: Affine3A::from_scale_rotation_translation(
                Vec3::ONE * panel.layout.content_size.x * PIXELS_TO_METERS,
                Quat::IDENTITY,
                vec3(-0.18, 0.12, -0.6),
            ),
            ..OverlayWindowState::default()
        },
        global: true,
        z_order: Z_ORDER_HELP,
        show_on_spawn: true,
        ..OverlayWindowConfig::from_backend(Box::new(panel))
    })
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, rc::Rc};

    use wgui::{
        assets::{AssetPathRef, AssetProvider},
        font_config::WguiFontConfig,
        globals::WguiGlobals,
        layout::LayoutParams,
        parser::ParseDocumentParams,
        theme::WguiTheme,
    };
    use wlx_common::{locale::WayVRLangProvider, palette::load_palette};

    use super::*;
    use crate::gui::asset::GuiAsset;

    const TEMPLATE: &str = "gui/lvrr/perf_overlay.xml";

    /// The template parses, has every label `FIELDS` updates, and its strings exist
    #[test]
    fn template() {
        let globals = WguiGlobals::new(
            Box::new(GuiAsset {}),
            &WayVRLangProvider::default(),
            &WguiFontConfig::default(),
            PathBuf::new(),
            load_palette("Default"),
        )
        .unwrap();
        let (_layout, state) = wgui::parser::new_layout_from_assets(
            &ParseDocumentParams {
                globals,
                path: AssetPathRef::BuiltIn(TEMPLATE),
                extra: Default::default(),
            },
            LayoutParams {
                resize_to_parent: false,
                theme: Rc::new(WguiTheme::default()),
            },
        )
        .unwrap();
        for (id, _) in FIELDS {
            assert!(state.get_widget_id(id).is_ok(), "{TEMPLATE} has no {id}");
        }

        let xml = String::from_utf8(GuiAsset {}.load_from_path(TEMPLATE).unwrap()).unwrap();
        let en: serde_json::Value =
            serde_json::from_slice(&GuiAsset {}.load_from_path("lang/en.json").unwrap()).unwrap();
        for (start, _) in xml.match_indices("\"LVRR.") {
            let key = &xml[start + 1..];
            let key = &key[..key.find('"').unwrap()];
            let value = key.split('.').try_fold(&en, |v, k| v.get(k));
            assert!(
                value.is_some_and(serde_json::Value::is_string),
                "{key} is missing from en.json"
            );
        }
    }
}
