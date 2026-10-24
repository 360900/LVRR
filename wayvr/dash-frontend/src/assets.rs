#[derive(rust_embed::Embed)]
#[folder = "assets/"]
pub struct Asset;

/// LVRR's replacements and additions, looked up before `Asset`.
#[derive(rust_embed::Embed)]
#[folder = "lvrr-assets/"]
struct LvrrAsset;

impl wgui::assets::AssetProvider for Asset {
	fn load_from_path(&mut self, path: &str) -> anyhow::Result<Vec<u8>> {
		lvrr_common::rebrand::load(
			path,
			|p| LvrrAsset::get(p).map(|f| f.data),
			|p| Asset::get(p).map(|f| f.data),
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use wgui::assets::AssetProvider;

	#[test]
	fn lvrr_branding() {
		for file in Asset::iter().filter(|f| f.starts_with("lang/") && f.ends_with(".json")) {
			let data = Asset.load_from_path(&file).unwrap();
			let root: serde_json::Value = serde_json::from_slice(&data).unwrap();
			for (key, value) in root.as_object().unwrap() {
				if key != "DONATE" {
					assert!(!value.to_string().contains("WayVR"), "{file}: {key} still says WayVR");
				}
			}
		}
		let splash = Asset.load_from_path("dashboard/splash.png").unwrap();
		assert_eq!(splash, LvrrAsset::get("dashboard/splash.png").unwrap().data.as_ref());
	}
}
