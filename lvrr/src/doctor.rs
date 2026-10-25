//! `lvrr doctor`: checks that the headset can reach this PC, and fixes what it can.
//!
//! Opening the firewall needs admin rights, so `lvrr doctor --fix` and the first start run
//! `lvrr setup-firewall` through pkexec. Checks read the rules without admin rights.

use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

/// The WiVRn server's port, TCP and UDP
const PORT: u16 = 9757;
/// mDNS, which the headset uses to find the server
const MDNS: u16 = 5353;

const UFW_CONF: &str = "/etc/ufw/ufw.conf";
const UFW_RULES: [&str; 2] = ["/etc/ufw/user.rules", "/etc/ufw/user6.rules"];
const UFW_PROFILE: &str = "/etc/ufw/applications.d/lvrr";
const UFW_PROFILE_TEXT: &str = "[LVRR]
title=LVRR
description=LVRR VR streaming server and headset discovery
ports=9757|5353/udp
";

const FIX: &str = "lvrr doctor --fix";
/// Written once the first start asked to open the firewall, so it asks only once
const ASKED_FILE: &str = "firewall-asked";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

pub struct Check {
    pub id: &'static str,
    pub title: &'static str,
    pub status: Status,
    pub message: String,
    pub fix: Option<&'static str>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Firewall {
    Ufw,
    Firewalld,
}

impl Firewall {
    fn name(self) -> &'static str {
        match self {
            Firewall::Ufw => "ufw",
            Firewall::Firewalld => "firewalld",
        }
    }
}

/// What the firewall lets in
#[derive(Default, Debug, PartialEq, Eq)]
struct Open {
    tcp: bool,
    udp: bool,
    mdns: bool,
}

impl Open {
    fn missing(&self) -> Vec<String> {
        let mut missing = Vec::new();
        if !self.tcp {
            missing.push(format!("TCP port {PORT}"));
        }
        if !self.udp {
            missing.push(format!("UDP port {PORT}"));
        }
        if !self.mdns {
            missing.push(format!("mDNS (UDP port {MDNS})"));
        }
        missing
    }

    fn add(&mut self, protocol: &str, ports: &str) {
        let (tcp, udp) = match protocol {
            "any" => (true, true),
            "tcp" => (true, false),
            "udp" => (false, true),
            _ => (false, false),
        };
        if port_in(ports, PORT) {
            self.tcp |= tcp;
            self.udp |= udp;
        }
        if port_in(ports, MDNS) {
            self.mdns |= udp;
        }
    }
}

/// `ports` is `any`, a port, or a list of ports and ranges (`9000:9999` or `9000-9999`)
fn port_in(ports: &str, port: u16) -> bool {
    ports == "any"
        || ports.split(',').any(|p| match p.split_once([':', '-']) {
            Some((low, high)) => {
                low.parse().is_ok_and(|low: u16| low <= port)
                    && high.parse().is_ok_and(|high: u16| port <= high)
            }
            None => p.parse() == Ok(port),
        })
}

/// Incoming rules from ufw's `### tuple ###` lines:
/// `action protocol dport dst sport src [dapp sapp] direction`
fn ufw_open(rules: &str) -> Open {
    let mut open = Open::default();
    for line in rules.lines() {
        let Some(tuple) = line.strip_prefix("### tuple ### ") else {
            continue;
        };
        let fields: Vec<&str> = tuple.split_whitespace().collect();
        let [action, protocol, dport, _dst, sport, ..] = fields[..] else {
            continue;
        };
        let incoming = fields.last().is_some_and(|d| d.starts_with("in"));
        // the headset sends from any port
        if matches!(action, "allow" | "limit") && incoming && sport == "any" {
            open.add(protocol, dport);
        }
    }
    open
}

/// Ports and services of a firewalld zone file
fn firewalld_zone_open(xml: &str) -> Open {
    let mut open = Open::default();
    for element in xml.split('<').skip(1) {
        if element.starts_with("port ") {
            if let (Some(port), Some(protocol)) =
                (attribute(element, "port"), attribute(element, "protocol"))
            {
                open.add(protocol, port);
            }
        } else if element.starts_with("service ") {
            match attribute(element, "name") {
                Some("wivrn" | "lvrr") => open.add("any", &PORT.to_string()),
                Some("mdns") => open.add("udp", &MDNS.to_string()),
                _ => (),
            }
        }
    }
    open
}

fn attribute<'a>(element: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let start = element.find(&key)? + key.len();
    element[start..].split('"').next()
}

fn active_firewall() -> Option<Firewall> {
    let ufw_enabled =
        fs::read_to_string(UFW_CONF).is_ok_and(|c| c.lines().any(|l| l.trim() == "ENABLED=yes"));
    if ufw_enabled {
        return Some(Firewall::Ufw);
    }
    let firewalld = Command::new("systemctl")
        .args(["--quiet", "is-active", "firewalld"])
        .status()
        .is_ok_and(|s| s.success());
    firewalld.then_some(Firewall::Firewalld)
}

fn read_open(firewall: Firewall) -> Result<Open, String> {
    match firewall {
        Firewall::Ufw => {
            let mut rules = String::new();
            for path in UFW_RULES {
                match fs::read_to_string(path) {
                    Ok(text) => rules.push_str(&text),
                    Err(e) => return Err(format!("{path}: {e}")),
                }
            }
            Ok(ufw_open(&rules))
        }
        Firewall::Firewalld => {
            let zone = fs::read_to_string("/etc/firewalld/firewalld.conf")
                .ok()
                .and_then(|c| {
                    c.lines()
                        .find_map(|l| l.strip_prefix("DefaultZone=").map(|z| z.trim().to_string()))
                })
                .unwrap_or_else(|| "public".to_string());
            let path = [
                format!("/etc/firewalld/zones/{zone}.xml"),
                format!("/usr/lib/firewalld/zones/{zone}.xml"),
            ]
            .into_iter()
            .find(|p| Path::new(p).exists())
            .ok_or_else(|| format!("no file for zone {zone}"))?;
            let xml = fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
            Ok(firewalld_zone_open(&xml))
        }
    }
}

pub fn firewall() -> Check {
    let (status, message) = match active_firewall() {
        None => (
            Status::Ok,
            "Neither ufw nor firewalld is on, nothing to open.".to_string(),
        ),
        Some(firewall) => match read_open(firewall) {
            Ok(open) if open.missing().is_empty() => (
                Status::Ok,
                format!(
                    "{} lets the headset in on port {PORT} and mDNS.",
                    firewall.name()
                ),
            ),
            Ok(open) => (
                Status::Fail,
                format!("{} blocks {}.", firewall.name(), open.missing().join(", ")),
            ),
            Err(e) => (
                Status::Warn,
                format!("Could not read the {} rules: {e}", firewall.name()),
            ),
        },
    };
    Check {
        id: "firewall",
        title: "Firewall",
        status,
        message,
        fix: (status != Status::Ok).then_some(FIX),
    }
}

/// `lvrr setup-firewall`, as root: opens the port and mDNS in the active firewall
pub fn setup_firewall() -> Result<(), String> {
    if !is_root() {
        return Err("needs admin rights, run `lvrr doctor --fix`".to_string());
    }
    match active_firewall() {
        None => Ok(()),
        Some(Firewall::Ufw) => {
            fs::write(UFW_PROFILE, UFW_PROFILE_TEXT)
                .map_err(|e| format!("writing {UFW_PROFILE}: {e}"))?;
            run("ufw", &["allow", "LVRR"])
        }
        Some(Firewall::Firewalld) => {
            let tcp = format!("--add-port={PORT}/tcp");
            let udp = format!("--add-port={PORT}/udp");
            run(
                "firewall-cmd",
                &["--permanent", &tcp, &udp, "--add-service=mdns"],
            )?;
            run("firewall-cmd", &["--reload"])
        }
    }
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| format!("running {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} {}: {status}", args.join(" ")))
    }
}

fn is_root() -> bool {
    fs::read_to_string("/proc/self/status").is_ok_and(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|uids| uids.split_whitespace().nth(1))
            == Some("0")
    })
}

/// Runs `lvrr setup-firewall` with admin rights; pkexec asks for the password
fn fix_firewall() -> Result<(), String> {
    let exe = env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|e| format!("locating the lvrr executable: {e}"))?;
    let status = Command::new("pkexec")
        .arg(exe)
        .arg("setup-firewall")
        .status()
        .map_err(|e| format!("running pkexec: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("pkexec lvrr setup-firewall: {status}"))
    }
}

/// On the first start, opens the firewall if it blocks the headset. Asks only once; after
/// that, `lvrr doctor` reports it.
pub fn first_start(config_dir: &Path) {
    let asked = config_dir.join(ASKED_FILE);
    if asked.exists() {
        return;
    }
    let check = firewall();
    if check.status == Status::Ok {
        return;
    }
    if let Err(e) = fs::write(&asked, "") {
        eprintln!("lvrr: writing {}: {e}", asked.display());
        return;
    }
    eprintln!(
        "lvrr: {} Opening it, which asks for an admin password.",
        check.message
    );
    if let Err(e) = fix_firewall() {
        eprintln!("lvrr: {e}. Run `{FIX}` to try again.");
    }
}

fn checks() -> Vec<Check> {
    vec![firewall()]
}

/// `lvrr doctor [--json] [--fix]`
pub fn main(args: &[String]) -> ExitCode {
    let mut json = false;
    let mut fix = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--fix" => fix = true,
            _ => {
                eprintln!("lvrr doctor: unknown argument {arg}");
                return ExitCode::FAILURE;
            }
        }
    }
    let mut checks = checks();
    if fix
        && checks
            .iter()
            .any(|c| c.id == "firewall" && c.status != Status::Ok)
    {
        if let Err(e) = fix_firewall() {
            eprintln!("lvrr: {e}");
        }
        checks = self::checks();
    }
    if json {
        println!("{}", to_json(&checks));
    } else {
        for c in &checks {
            println!("{:4}  {}: {}", c.status.name(), c.title, c.message);
            if let Some(fix) = c.fix {
                println!("      fix: {fix}");
            }
        }
    }
    if checks.iter().any(|c| c.status == Status::Fail) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// The report read by the dashboard, see `lvrr_common::doctor`
fn to_json(checks: &[Check]) -> String {
    let items: Vec<String> = checks
        .iter()
        .map(|c| {
            let fix = c
                .fix
                .map(|f| format!(",\"fix\":{}", json_string(f)))
                .unwrap_or_default();
            format!(
                "{{\"id\":{},\"title\":{},\"status\":\"{}\",\"message\":{}{fix}}}",
                json_string(c.id),
                json_string(c.title),
                c.status.name(),
                json_string(&c.message)
            )
        })
        .collect();
    format!("{{\"checks\":[{}]}}", items.join(","))
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // From a ufw with the ports opened by hand, and an mDNS rule by source port only
    const UFW: &str = "\
### tuple ### allow any any 0.0.0.0/0 5353 0.0.0.0/0 in
-A ufw-user-input -p udp --sport 5353 -j ACCEPT
### tuple ### allow any 9757 0.0.0.0/0 any 0.0.0.0/0 in
-A ufw-user-input -p tcp --dport 9757 -j ACCEPT
### tuple ### allow any any 0.0.0.0/0 9757 0.0.0.0/0 in
";

    #[test]
    fn ufw_rules() {
        assert_eq!(
            ufw_open(UFW),
            Open {
                tcp: true,
                udp: true,
                mdns: false
            }
        );
        let profile = "### tuple ### allow tcp 9757 0.0.0.0/0 any 0.0.0.0/0 LVRR - in\n\
                       ### tuple ### allow udp 9757,5353 0.0.0.0/0 any 0.0.0.0/0 LVRR - in\n";
        assert!(ufw_open(profile).missing().is_empty());
        let outgoing = "### tuple ### allow any 9757 0.0.0.0/0 any 0.0.0.0/0 out\n";
        assert_eq!(ufw_open(outgoing), Open::default());
        let range = "### tuple ### allow udp 5000:6000 0.0.0.0/0 any 192.168.1.0/24 in\n";
        assert!(ufw_open(range).mdns);
    }

    #[test]
    fn firewalld_zone() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<zone>
  <short>Public</short>
  <service name="ssh"/>
  <service name="mdns"/>
  <port port="9757" protocol="tcp"/>
  <port protocol="udp" port="9700-9800"/>
</zone>"#;
        assert!(firewalld_zone_open(xml).missing().is_empty());
        let service = r#"<zone><service name="wivrn"/></zone>"#;
        assert_eq!(
            firewalld_zone_open(service).missing(),
            ["mDNS (UDP port 5353)"]
        );
    }

    #[test]
    fn report_json() {
        let checks = [Check {
            id: "firewall",
            title: "Firewall",
            status: Status::Fail,
            message: "ufw blocks \"9757\"\n".to_string(),
            fix: Some(FIX),
        }];
        assert_eq!(
            to_json(&checks),
            r#"{"checks":[{"id":"firewall","title":"Firewall","status":"fail","message":"ufw blocks \"9757\"\n","fix":"lvrr doctor --fix"}]}"#
        );
    }
}
