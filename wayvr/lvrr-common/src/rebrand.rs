//! Shows LVRR instead of WayVR in user-visible strings without editing upstream files.
//!
//! Asset providers load LVRR's own assets (`lvrr-assets/` in each crate) before the upstream
//! ones, and pass language files through [`lang_json`].

use std::borrow::Cow;

use serde_json::Value;

const UPSTREAM_NAME: &str = "WayVR";
const NAME: &str = "LVRR";

/// Translation keys left as they are, because they credit the WayVR project.
const KEEP_PREFIXES: &[&str] = &["DONATE."];

/// Loads `path` from LVRR's assets if present, otherwise from upstream. Language files are
/// always built from upstream, rebranded, with LVRR's strings merged over them.
pub fn load(
	path: &str,
	lvrr: impl Fn(&str) -> Option<Cow<'static, [u8]>>,
	upstream: impl Fn(&str) -> Option<Cow<'static, [u8]>>,
) -> anyhow::Result<Vec<u8>> {
	if path.starts_with("lang/") && path.ends_with(".json") {
		let Some(base) = upstream(path) else {
			anyhow::bail!("embedded file {path} not found");
		};
		return lang_json(&base, lvrr(path).as_deref());
	}
	match lvrr(path).or_else(|| upstream(path)) {
		Some(data) => Ok(data.into_owned()),
		None => anyhow::bail!("embedded file {path} not found"),
	}
}

/// Rebrands an upstream language file and merges LVRR's strings (`overrides`) over it.
pub fn lang_json(upstream: &[u8], overrides: Option<&[u8]>) -> anyhow::Result<Vec<u8>> {
	let mut root: Value = serde_json::from_slice(upstream)?;
	rename(&mut root, "");
	if let Some(overrides) = overrides {
		merge(&mut root, serde_json::from_slice(overrides)?);
	}
	Ok(serde_json::to_vec(&root)?)
}

fn rename(value: &mut Value, key: &str) {
	match value {
		Value::String(s) if s.contains(UPSTREAM_NAME) && !KEEP_PREFIXES.iter().any(|p| key.starts_with(p)) => {
			*s = s.replace(UPSTREAM_NAME, NAME);
		}
		Value::Object(map) => {
			for (k, v) in map.iter_mut() {
				let child = if key.is_empty() {
					k.clone()
				} else {
					format!("{key}.{k}")
				};
				rename(v, &child);
			}
		}
		_ => {}
	}
}

fn merge(base: &mut Value, overrides: Value) {
	match (base, overrides) {
		(Value::Object(base), Value::Object(overrides)) => {
			for (k, v) in overrides {
				match base.get_mut(&k) {
					Some(existing) => merge(existing, v),
					None => {
						base.insert(k, v);
					}
				}
			}
		}
		(base, overrides) => *base = overrides,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn rebrand(upstream: &str, overrides: Option<&str>) -> Value {
		let out = lang_json(upstream.as_bytes(), overrides.map(str::as_bytes)).unwrap();
		serde_json::from_slice(&out).unwrap()
	}

	#[test]
	fn renames_but_keeps_credits() {
		let out = rebrand(
			r#"{"WELCOME": {"THANKS": "Thank you for installing WayVR!"}, "DONATE": {"TEAM": "From the WayVR Team"}}"#,
			None,
		);
		assert_eq!(out["WELCOME"]["THANKS"], "Thank you for installing LVRR!");
		assert_eq!(out["DONATE"]["TEAM"], "From the WayVR Team");
	}

	#[test]
	fn merges_overrides() {
		let out = rebrand(
			r#"{"A": {"B": "WayVR", "C": "kept"}}"#,
			Some(r#"{"A": {"B": "Restart LVRR desktop"}, "LVRR": {"NEW": "New string"}}"#),
		);
		assert_eq!(out["A"]["B"], "Restart LVRR desktop");
		assert_eq!(out["A"]["C"], "kept");
		assert_eq!(out["LVRR"]["NEW"], "New string");
	}

	#[test]
	fn prefers_lvrr_assets() {
		let lvrr = |p: &str| (p == "a.svg").then(|| Cow::Borrowed(&b"lvrr"[..]));
		let upstream = |_: &str| Some(Cow::Borrowed(&b"upstream"[..]));
		assert_eq!(load("a.svg", lvrr, upstream).unwrap(), b"lvrr");
		assert_eq!(load("b.svg", lvrr, upstream).unwrap(), b"upstream");
		assert!(load("lang/en.json", |_| None, |_| None).is_err());
	}
}
