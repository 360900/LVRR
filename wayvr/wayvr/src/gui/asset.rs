#[derive(rust_embed::Embed)]
#[folder = "src/assets/"]
pub struct GuiAsset;

/// LVRR's replacements and additions, looked up before `GuiAsset`.
#[derive(rust_embed::Embed)]
#[folder = "lvrr-assets/"]
struct LvrrAsset;

impl wgui::assets::AssetProvider for GuiAsset {
    fn load_from_path(&mut self, path: &str) -> anyhow::Result<Vec<u8>> {
        lvrr_common::rebrand::load(
            path,
            |p| LvrrAsset::get(p).map(|f| f.data),
            |p| Self::get(p).map(|f| f.data),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgui::assets::AssetProvider;

    #[test]
    fn lvrr_branding() {
        for file in GuiAsset::iter().filter(|f| f.starts_with("lang/") && f.ends_with(".json")) {
            let data = GuiAsset.load_from_path(&file).unwrap();
            assert!(
                !String::from_utf8_lossy(&data).contains("WayVR"),
                "{file} still says WayVR"
            );
        }
        let icon = GuiAsset
            .load_from_path("watch/wayvr_dashboard_mono.svg")
            .unwrap();
        assert_eq!(
            icon,
            LvrrAsset::get("watch/wayvr_dashboard_mono.svg")
                .unwrap()
                .data
                .as_ref()
        );
    }
}
