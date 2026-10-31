//! The environment the running Steam client passes to its games, which decides the launch options they need.
//! `lvrr steam` and `lvrr doctor` read it the same way (lvrr/src/headset.rs, checks.rs).

use std::fs;

/// Lets pressure-vessel pass LVRR's OpenXR runtime into Steam's game sandbox
const OPENXR_IMPORT: &str = "PRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1";

/// Whether the running Steam client has `PRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1`, which its games inherit; None
/// when none runs or its environment can't be read. Every install runs the client as ubuntu12_32/steam, and only this
/// user's processes show their executable. It reads /proc, so call it when a page is built, not every frame.
pub fn client_import() -> Option<bool> {
	let pid = fs::read_dir("/proc")
		.ok()?
		.flatten()
		.filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
		.find(|pid| steam_client(*pid))?;
	let environ = fs::read(format!("/proc/{pid}/environ")).ok()?;
	Some(environ.split(|b| *b == 0).any(|var| var == OPENXR_IMPORT.as_bytes()))
}

fn steam_client(pid: u32) -> bool {
	fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| {
		let exe = exe.to_string_lossy();
		// an update replaces the file under the running client
		let exe = exe.strip_suffix(" (deleted)").unwrap_or(&exe);
		exe.ends_with("/ubuntu12_32/steam")
	})
}

/// `command`, the server's launch options, with the import first when the running Steam lacks it (`steam`, see
/// client_import()): its games get its environment, whatever the server's has. `lvrr steam` applies the same rule.
pub fn with_openxr_import(command: &str, steam: Option<bool>) -> String {
	if steam != Some(false) || command.contains(OPENXR_IMPORT) {
		command.to_string()
	} else if command.is_empty() {
		format!("{OPENXR_IMPORT} %command%")
	} else {
		format!("{OPENXR_IMPORT} {command}")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn openxr_import() {
		let set = "PRESSURE_VESSEL_IMPORT_OPENXR_1_RUNTIMES=1";
		let with = with_openxr_import;
		// the running Steam lacks it
		assert_eq!(with("", Some(false)), format!("{set} %command%"));
		let compat = "VR_OVERRIDE=/run/host/usr/lib/xrizer %command%";
		assert_eq!(with(compat, Some(false)), format!("{set} {compat}"));
		let server = format!("{set} {compat}");
		assert_eq!(with(&server, Some(false)), server);
		// it has it, or none runs
		assert_eq!(with("", Some(true)), "");
		assert_eq!(with(compat, None), compat);
	}
}
