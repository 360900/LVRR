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

	/// These credit the WayVR project
	const CREDITS: &[&str] = &["DONATE.", "LVRR.PC.CREDITS", "LVRR.PC.SUPPORT_UPSTREAM"];

	fn assert_rebranded(file: &str, key: &str, value: &serde_json::Value) {
		match value {
			serde_json::Value::Object(map) => {
				for (k, v) in map {
					let child = if key.is_empty() {
						k.clone()
					} else {
						format!("{key}.{k}")
					};
					assert_rebranded(file, &child, v);
				}
			}
			serde_json::Value::String(s) if !CREDITS.iter().any(|c| key.starts_with(c)) => {
				assert!(!s.contains("WayVR"), "{file}: {key} still says WayVR");
			}
			_ => {}
		}
	}

	#[test]
	fn lvrr_branding() {
		for file in Asset::iter().filter(|f| f.starts_with("lang/") && f.ends_with(".json")) {
			let data = Asset.load_from_path(&file).unwrap();
			let root: serde_json::Value = serde_json::from_slice(&data).unwrap();
			assert_rebranded(&file, "", &root);
		}
		let splash = Asset.load_from_path("dashboard/splash.png").unwrap();
		assert_eq!(splash, LvrrAsset::get("dashboard/splash.png").unwrap().data.as_ref());
	}
}
