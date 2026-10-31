//! System checks reported by `lvrr doctor --json`, and their fixes.

use anyhow::Context;
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
	/// Why fixes failed, from `lvrr doctor --fix`
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub errors: Vec<String>,
}

impl Report {
	/// Runs `lvrr doctor --json`; blocks until it is done
	pub fn run() -> anyhow::Result<Self> {
		Self::doctor(&["--json"])
	}

	/// Runs `lvrr doctor --fix ID --json`, which fixes the check `id` if it isn't OK and has a fix,
	/// then runs the checks; blocks until it is done. A fix that needs admin rights asks for the
	/// password on the PC's screen.
	pub fn fix(id: &str) -> anyhow::Result<Self> {
		Self::doctor(&["--fix", id, "--json"])
	}

	fn doctor(args: &[&str]) -> anyhow::Result<Self> {
		let lvrr = crate::lvrr_command();
		let output = std::process::Command::new(&lvrr)
			.arg("doctor")
			.args(args)
			.output()
			.with_context(|| format!("running {}", lvrr.display()))?;
		let stderr = String::from_utf8_lossy(&output.stderr);
		let stderr = stderr.trim();
		// It exits with an error when a check fails, the report says which. Without a report, stderr
		// says why.
		let report: Self = serde_json::from_slice(&output.stdout).map_err(|e| {
			let why = stderr.lines().next().map_or_else(|| e.to_string(), str::to_string);
			anyhow::anyhow!("{} doctor failed ({}): {why}", lvrr.display(), output.status)
		})?;
		// A failed fix's error is a few words, for a toast. What pkexec and the tools the fix ran printed
		// says why, for the log.
		if !report.errors.is_empty() && !stderr.is_empty() {
			log::warn!("{} doctor {}: {stderr}", lvrr.display(), args.join(" "));
		}
		Ok(report)
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
		assert!(report.errors.is_empty());
		// `lvrr doctor --fix --json` adds why fixes failed
		let fixed: Report =
			serde_json::from_str(r#"{"checks": [], "errors": ["the admin password prompt was closed"]}"#).unwrap();
		assert_eq!(fixed.errors, ["the admin password prompt was closed"]);
	}
}
