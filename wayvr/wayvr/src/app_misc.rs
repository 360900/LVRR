use wgui::layout::LayoutUpdateResult;

use crate::state::AppState;

pub fn process_layout_result(app: &mut AppState, res: LayoutUpdateResult) {
    app.audio.play_wgui_samples(res.sounds_to_play);
}
