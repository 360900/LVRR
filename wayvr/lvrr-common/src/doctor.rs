//! System checks reported by `lvrr doctor --json`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
	Ok,
	Warn,
	Fail,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
	pub id: String,
	pub title: String,
	pub status: Status,
	pub message: String,
	/// What to do about it, if anything
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub fix: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
	pub checks: Vec<Check>,
}

impl Report {
	/// Runs `lvrr doctor --json`; blocks until it is done
	pub fn run() -> anyhow::Result<Self> {
		let output = std::process::Command::new("lvrr").args(["doctor", "--json"]).output()?;
		Ok(serde_json::from_slice(&output.stdout)?)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_report() {
		let report: Report = serde_json::from_str(
			r#"{"checks": [{"id": "avahi", "title": "Avahi", "status": "ok", "message": "running"},
			{"id": "firewall", "title": "Firewall", "status": "fail", "message": "port 9757 closed", "fix": "lvrr doctor --fix"}]}"#,
		)
		.unwrap();
		assert_eq!(report.checks[1].status, Status::Fail);
		assert_eq!(report.checks[1].fix.as_deref(), Some("lvrr doctor --fix"));
		assert_eq!(report.checks[0].fix, None);
	}
}
