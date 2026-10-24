//! `cargo xtask rebrand`: shows LVRR instead of WiVRn and WayVR to users.
//!
//! Only user-visible strings and images change; identifiers (binaries, D-Bus names, config
//! directories) keep their upstream names. Re-run it after merging an upstream: it is
//! idempotent, and `--check` reports what still needs rebranding, rules that no longer match,
//! and upstream images that changed under LVRR's replacements.
//!
//! WayVR's language files are rebranded when they are loaded (see
//! `wayvr/lvrr-common/src/rebrand.rs`), so they never need editing here.

use std::fs;
use std::process::Command;

use crate::{Result, root};

/// `from` replaced by `to` everywhere in `file`.
struct Rule {
    file: &'static str,
    from: &'static str,
    to: &'static str,
}

const fn rule(file: &'static str, from: &'static str, to: &'static str) -> Rule {
    Rule { file, from, to }
}

const RULES: &[Rule] = &[
    // Headset app
    rule(
        "wivrn/client/render/ui_widgets.cpp",
        "ImGui::TextUnformatted(\"WiVRn\");",
        "ImGui::TextUnformatted(\"LVRR\");",
    ),
    rule(
        "wivrn/client/scenes/lobby_gui.cpp",
        "std::string(\"WiVRn \")",
        "std::string(\"LVRR \")",
    ),
    // Server: names shown to OpenXR apps, in mixers, notifications and the terminal
    rule(
        "wivrn/server/dist/openxr_manifest.in.json",
        "\"name\": \"WiVRn\",",
        "\"name\": \"LVRR\",",
    ),
    rule(
        "wivrn/server/u_git_tag.cpp.in",
        "u_runtime_description[] = \"WiVRn\";",
        "u_runtime_description[] = \"LVRR\";",
    ),
    rule(
        "wivrn/server/driver/wivrn_session.cpp",
        "\" on WiVRn\"",
        "\" on LVRR\"",
    ),
    rule(
        "wivrn/server/driver/wivrn_session.cpp",
        "\"WiVRn(microphone)\",",
        "\"LVRR (microphone)\",",
    ),
    rule(
        "wivrn/server/driver/wivrn_session.cpp",
        "\"WiVRn\",",
        "\"LVRR\",",
    ),
    rule(
        "wivrn/server/driver/wivrn_hmd.cpp",
        ".str = \"WiVRn HMD\",",
        ".str = \"LVRR HMD\",",
    ),
    rule(
        "wivrn/server/driver/wivrn_uinput.cpp",
        "\"WiVRn Keyboard\"",
        "\"LVRR Headset Keyboard\"",
    ),
    rule(
        "wivrn/server/driver/wivrn_uinput.cpp",
        "\"WiVRn Mouse\"",
        "\"LVRR Headset Mouse\"",
    ),
    rule(
        "wivrn/server/driver/wivrn_uinput.cpp",
        "\"WiVRn Gamepad\"",
        "\"LVRR Headset Gamepad\"",
    ),
    rule(
        "wivrn/server/sleep_inhibitor.cpp",
        "\"WiVRn\",                     // Who",
        "\"LVRR\",                      // Who",
    ),
    rule(
        "wivrn/server/sleep_inhibitor.cpp",
        "\"A WiVRn session is active\"",
        "\"An LVRR session is active\"",
    ),
    rule(
        "wivrn/server/avahi_publisher.cpp",
        "avahi_strdup(\"WiVRn\")",
        "avahi_strdup(\"LVRR\")",
    ),
    rule(
        "wivrn/server/start_systemd_unit.cpp",
        "\"Application spawned by WiVRn\"",
        "\"Application spawned by LVRR\"",
    ),
    rule(
        "wivrn/server/main.cpp",
        "\"If WiVRn is not running, delete \"",
        "\"If LVRR is not running, delete \"",
    ),
    rule(
        "wivrn/server/main.cpp",
        "std::cerr << \"WiVRn \" <<",
        "std::cerr << \"LVRR \" <<",
    ),
    rule(
        "wivrn/server/main.cpp",
        "notify_init(\"WiVRn\")",
        "notify_init(\"LVRR\")",
    ),
    rule(
        "wivrn/server/main.cpp",
        "std::cout << \"WiVRn version \"",
        "std::cout << \"LVRR version \"",
    ),
    // WayVR: notifications, CLI help, names shown by other software
    rule(
        "wayvr/wayvr/src/main.rs",
        "WayVR may have issues",
        "LVRR may have issues",
    ),
    rule(
        "wayvr/wayvr/src/main.rs",
        "Replace running WayVR instance",
        "Replace running LVRR desktop instance",
    ),
    rule(
        "wayvr/wayvr/src/main.rs",
        "instances of WayVR",
        "instances of the LVRR desktop",
    ),
    rule(
        "wayvr/wayvr/src/main.rs",
        "Looks like WayVR is already running.",
        "Looks like the LVRR desktop is already running.",
    ),
    rule(
        "wayvr/wayvr/src/main.rs",
        "notify_send(\"WayVR\",",
        "notify_send(\"LVRR\",",
    ),
    rule(
        "wayvr/wayvr/src/overlays/screen/pw.rs",
        "notify_send(\"WayVR\",",
        "notify_send(\"LVRR\",",
    ),
    rule(
        "wayvr/wayvr/src/backend/wayvr/mod.rs",
        "notify_send(\"WayVR\",",
        "notify_send(\"LVRR\",",
    ),
    rule(
        "wayvr/wayvr/src/backend/wayvr/mod.rs",
        "\"Virtual WayVR Display\"",
        "\"Virtual LVRR Display\"",
    ),
    rule(
        "wayvr/wayvr/src/subsystem/dbus/mod.rs",
        "\"WayVR\",",
        "\"LVRR\",",
    ),
    // Not IGNORE_PREFIX ("WayVR";), which must match the uinput device names
    rule(
        "wayvr/wayvr/src/backend/wayvr/input_capture.rs",
        "\"WayVR\",",
        "\"LVRR\",",
    ),
    rule(
        "wayvr/wayvr/src/backend/openvr/manifest.rs",
        "name: \"WayVR\",",
        "name: \"LVRR\",",
    ),
    rule(
        "wayvr/wayvr/src/backend/openxr/input.rs",
        "\"WayVR Actions\"",
        "\"LVRR Actions\"",
    ),
    // Follows the server's audio device name above
    rule(
        "wayvr/dash-frontend/src/views/audio_settings.rs",
        "device_desc.contains(\"WiVRn\")",
        "device_desc.contains(\"LVRR\")",
    ),
];

/// Translatable headset strings: changed in the sources and, with their translations, in
/// every `wivrn/locale/*/wivrn.po`.
const MESSAGES: &[(&[&str], &str, &str)] = &[
    (
        &["wivrn/client/scenes/gui_settings.cpp"],
        "WiVRn can't directly change the refresh rate on this device. See your device's settings to change the refresh rate.",
        "LVRR can't directly change the refresh rate on this device. See your device's settings to change the refresh rate.",
    ),
    (
        &["wivrn/client/scenes/lobby_gui.cpp"],
        "Pick a PC running the WiVRn server to stream from.",
        "Pick a PC running the LVRR server to stream from.",
    ),
    (
        &["wivrn/client/scenes/lobby_gui.cpp"],
        "Start a WiVRn server on your local network.",
        "Start an LVRR server on your local network.",
    ),
    (
        &["wivrn/client/scenes/lobby_gui.cpp"],
        "Welcome to WiVRn",
        "Welcome to LVRR",
    ),
    (
        &["wivrn/client/scenes/lobby_gui.cpp"],
        "Open-source components bundled with WiVRn.",
        "Open-source components bundled with LVRR.",
    ),
    (
        &["wivrn/client/scenes/stream_gui.cpp"],
        "Press both thumbsticks to display the WiVRn window",
        "Press both thumbsticks to display the LVRR window",
    ),
    (
        &["wivrn/client/scenes/lobby.cpp"],
        "Incompatible WiVRn server: no protocol field in TXT",
        "Incompatible LVRR server: no protocol field in TXT",
    ),
    (
        &["wivrn/client/scenes/lobby.cpp"],
        "Incompatible WiVRn server protocol (client: {}, server: {})",
        "Incompatible LVRR server protocol (client: {}, server: {})",
    ),
];

/// LVRR's logo, copied over upstream images.
const COPIES: &[(&str, &str)] = &[
    ("packaging/logo/lvrr.svg", "wivrn/images/wivrn.svg"),
    ("packaging/logo/lvrr.svg", "wivrn/images/wivrn-pride.svg"),
    (
        "packaging/logo/lvrr.svg",
        "wivrn/images/wivrn-christmas.svg",
    ),
    (
        "packaging/logo/lvrr-mono.svg",
        "wayvr/dash-frontend/lvrr-assets/dashboard/wayvr_dashboard_mono.svg",
    ),
    (
        "packaging/logo/lvrr-mono.svg",
        "wayvr/wayvr/lvrr-assets/watch/wayvr_dashboard_mono.svg",
    ),
];

/// Upstream images LVRR's assets replace, with the git blob ids they were replaced at, in
/// `packaging/rebrand-upstream.txt`. A changed blob means upstream changed the image.
const REPLACED: &[&str] = &[
    "wayvr/dash-frontend/assets/dashboard/splash.png",
    "wayvr/dash-frontend/assets/dashboard/wayvr_dashboard_mono.svg",
    "wayvr/wayvr/src/assets/watch/wayvr_dashboard_mono.svg",
];
const REPLACED_RECORD: &str = "packaging/rebrand-upstream.txt";

/// Headset sources scanned for translatable strings that still say WiVRn.
const SCANNED_SOURCES: &str = "wivrn/client";
const TRANSLATABLE: &[&str] = &["_(\"", "_S(\"", "_F(\"", "_cS(", "_cF(", "_C("];

pub fn run(args: &[String]) -> Result {
    let check = match args {
        [] => false,
        [a] if a == "--check" => true,
        _ => {
            return Err(format!(
                "rebrand: unknown arguments {args:?}\n\n{}",
                crate::USAGE
            ));
        }
    };
    let mut problems = Vec::new();
    let mut changed = Vec::new();

    for r in RULES {
        let text = read(r.file)?;
        if text.contains(r.from) {
            changed.push(r.file.to_string());
            if !check {
                write(r.file, &text.replace(r.from, r.to))?;
            }
        } else if !text.contains(r.to) {
            problems.push(format!("{}: rule no longer matches: {:?}", r.file, r.from));
        }
    }

    for (files, from, to) in MESSAGES {
        let (from_lit, to_lit) = (format!("\"{from}\""), format!("\"{to}\""));
        for file in *files {
            let text = read(file)?;
            if text.contains(&from_lit) {
                changed.push(file.to_string());
                if !check {
                    write(file, &text.replace(&from_lit, &to_lit))?;
                }
            } else if !text.contains(&to_lit) {
                problems.push(format!("{file}: message no longer found: {from:?}"));
            }
        }
    }

    for po in po_files()? {
        let text = read(&po)?;
        let rebranded = rebrand_po(&text);
        if rebranded != text {
            changed.push(po.clone());
            if !check {
                write(&po, &rebranded)?;
            }
        }
    }

    for (src, dst) in COPIES {
        let data = fs::read(root().join(src)).map_err(|e| format!("{src}: {e}"))?;
        if fs::read(root().join(dst)).ok().as_deref() != Some(&data[..]) {
            changed.push(dst.to_string());
            if !check {
                fs::write(root().join(dst), &data).map_err(|e| format!("{dst}: {e}"))?;
            }
        }
    }

    let record = replaced_record()?;
    if check {
        let recorded = fs::read_to_string(root().join(REPLACED_RECORD)).unwrap_or_default();
        if recorded != record {
            problems.push(format!(
				"upstream changed an image LVRR replaces, or {REPLACED_RECORD} is missing; review the replacement in lvrr-assets, then run `cargo xtask rebrand`"
			));
        }
    } else {
        write(REPLACED_RECORD, &record)?;
    }

    problems.extend(unbranded_messages()?);

    changed.sort();
    changed.dedup();
    for file in &changed {
        println!(
            "{} {file}",
            if check {
                "needs rebrand:"
            } else {
                "rebranded:"
            }
        );
    }
    for p in &problems {
        eprintln!("xtask: {p}");
    }
    if !problems.is_empty() || (check && !changed.is_empty()) {
        return Err("rebrand: see above".into());
    }
    Ok(())
}

/// Rebrands the entries of a gettext catalog whose msgid is one of `MESSAGES`: the msgid
/// becomes the new source string, and the translations get the new name.
fn rebrand_po(text: &str) -> String {
    let mut out = Vec::new();
    for entry in text.split("\n\n") {
        let lines: Vec<&str> = entry.lines().collect();
        let Some(start) = lines.iter().position(|l| l.starts_with("msgid \"")) else {
            out.push(entry.to_string());
            continue;
        };
        let mut end = start + 1;
        while end < lines.len() && lines[end].starts_with('"') {
            end += 1;
        }
        let msgid: String = lines[start..end]
            .iter()
            .map(|l| l.trim_start_matches("msgid ").trim_matches('"'))
            .collect();
        let Some((_, _, to)) = MESSAGES.iter().find(|(_, from, _)| *from == msgid) else {
            out.push(entry.to_string());
            continue;
        };
        let mut rebranded: Vec<String> = lines[..start].iter().map(|l| l.to_string()).collect();
        rebranded.push(format!("msgid \"{to}\""));
        for line in &lines[end..] {
            rebranded.push(line.replace("WiVRn", "LVRR").replace("WinVRn", "LVRR"));
        }
        let mut rebranded = rebranded.join("\n");
        if entry.ends_with('\n') {
            rebranded.push('\n');
        }
        out.push(rebranded);
    }
    out.join("\n\n")
}

fn po_files() -> Result<Vec<String>> {
    let dir = root().join("wivrn/locale");
    let mut files = Vec::new();
    for lang in fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
    {
        let po = lang.path().join("wivrn.po");
        if po.exists() {
            let rel = po.strip_prefix(root()).unwrap();
            files.push(rel.to_string_lossy().into_owned());
        }
    }
    files.sort();
    Ok(files)
}

fn replaced_record() -> Result<String> {
    let mut record = String::new();
    for file in REPLACED {
        let out = Command::new("git")
            .current_dir(root())
            .args(["hash-object", file])
            .output()
            .map_err(|e| format!("running git: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git hash-object {file}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        record.push_str(&format!(
            "{} {file}\n",
            String::from_utf8_lossy(&out.stdout).trim()
        ));
    }
    Ok(record)
}

/// Translatable headset strings mentioning WiVRn that no message rule covers, typically new
/// upstream strings.
fn unbranded_messages() -> Result<Vec<String>> {
    let mut found = Vec::new();
    let mut stack = vec![root().join(SCANNED_SOURCES)];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !matches!(path.extension().and_then(|e| e.to_str()), Some("cpp" | "h")) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for (n, line) in text.lines().enumerate() {
                if line.contains("WiVRn") && TRANSLATABLE.iter().any(|t| line.contains(t)) {
                    let rel = path.strip_prefix(root()).unwrap().display();
                    found.push(format!(
                        "{rel}:{}: translatable string still says WiVRn",
                        n + 1
                    ));
                }
            }
        }
    }
    found.sort();
    Ok(found)
}

fn read(file: &str) -> Result<String> {
    fs::read_to_string(root().join(file)).map_err(|e| format!("{file}: {e}"))
}

fn write(file: &str, text: &str) -> Result {
    let path = root().join(file);
    if fs::read_to_string(&path).ok().as_deref() == Some(text) {
        return Ok(());
    }
    fs::write(&path, text).map_err(|e| format!("{file}: {e}"))
}
