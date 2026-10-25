//! `lvrr doctor`: checks that the headset can reach this PC, and fixes what it can.
//!
//! Opening the firewall needs admin rights, so `lvrr doctor --fix` and the first start run
//! `lvrr setup-firewall` through pkexec. Checks read the rules without admin rights.

use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

use crate::bus::{self, Server};
use crate::checks;
use crate::server::{self, State};

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

pub const FIX: &str = "lvrr doctor --fix";
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

/// Ports and services of a firewalld zone file. A zone that accepts
/// everything is open.
fn firewalld_zone_open(xml: &str) -> Open {
    let mut open = Open::default();
    for element in xml.split('<').skip(1) {
        if element.starts_with("zone ") && attribute(element, "target") == Some("ACCEPT") {
            open.add("any", "any");
        } else if element.starts_with("port ") {
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

fn read_ufw() -> Result<Open, String> {
    let mut rules = String::new();
    for path in UFW_RULES {
        match fs::read_to_string(path) {
            Ok(text) => rules.push_str(&text),
            Err(e) => return Err(format!("{path}: {e}")),
        }
    }
    Ok(ufw_open(&rules))
}

fn default_zone() -> String {
    fs::read_to_string("/etc/firewalld/firewalld.conf")
        .ok()
        .and_then(|c| {
            c.lines()
                .find_map(|l| l.strip_prefix("DefaultZone=").map(|z| z.trim().to_string()))
        })
        .filter(|z| !z.is_empty())
        .unwrap_or_else(|| "public".to_string())
}

/// The zones `firewall-cmd --get-active-zones` lists, with their interfaces
fn active_zones(text: &str) -> Vec<(String, Vec<String>)> {
    let mut zones: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            if let Some(zone) = line.split_whitespace().next() {
                zones.push((zone.to_string(), Vec::new()));
            }
        } else if let (Some(interfaces), Some((_, list))) =
            (line.trim().strip_prefix("interfaces:"), zones.last_mut())
        {
            list.extend(interfaces.split_whitespace().map(str::to_string));
        }
    }
    zones
}

/// Interfaces with a default route, from /proc/net/route and ipv6_route: the
/// ones the headset's network is on
fn default_route_interfaces(ipv4: &str, ipv6: &str) -> Vec<String> {
    let v4 = ipv4.lines().skip(1).filter_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.get(1) == Some(&"00000000") && fields.get(7) == Some(&"00000000"))
            .then(|| fields[0].to_string())
    });
    let v6 = ipv6.lines().filter_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let default = fields.first().is_some_and(|d| d.bytes().all(|b| b == b'0'))
            && fields.get(1) == Some(&"00");
        default
            .then(|| fields.last().map(|i| i.to_string()))
            .flatten()
    });
    let mut interfaces: Vec<String> = v4.chain(v6).filter(|i| i != "lo").collect();
    interfaces.sort();
    interfaces.dedup();
    interfaces
}

/// The firewalld zones of the interfaces with a default route. firewalld puts
/// an interface no zone names in the default zone.
fn firewalld_zones() -> Vec<String> {
    let default = default_zone();
    let active = Command::new("firewall-cmd")
        .arg("--get-active-zones")
        .output()
        .map(|o| active_zones(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    let interfaces = default_route_interfaces(
        &fs::read_to_string("/proc/net/route").unwrap_or_default(),
        &fs::read_to_string("/proc/net/ipv6_route").unwrap_or_default(),
    );
    let mut zones: Vec<String> = interfaces
        .iter()
        .map(|interface| {
            active
                .iter()
                .find(|(_, interfaces)| interfaces.contains(interface))
                .map_or_else(|| default.clone(), |(zone, _)| zone.clone())
        })
        .collect();
    if zones.is_empty() {
        zones.push(default);
    }
    zones.sort();
    zones.dedup();
    zones
}

fn read_zone(zone: &str) -> Result<Open, String> {
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

/// Firewall services with hand-written rules, which only root can read
fn rules_services() -> Vec<&'static str> {
    ["nftables", "iptables", "ip6tables"]
        .into_iter()
        .filter(|service| {
            Command::new("systemctl")
                .args(["--quiet", "is-active", &format!("{service}.service")])
                .status()
                .is_ok_and(|s| s.success())
        })
        .collect()
}

/// What to add to hand-written rules. Inserted first, so a reject rule
/// before the end doesn't win.
fn rules_lines(services: &[&str]) -> Vec<String> {
    let mut lines = Vec::new();
    for service in services {
        match *service {
            "nftables" => {
                lines.push(format!(
                    "nft insert rule inet filter input tcp dport {PORT} accept"
                ));
                lines.push(format!(
                    "nft insert rule inet filter input udp dport {{ {PORT}, {MDNS} }} accept"
                ));
            }
            tool => {
                lines.push(format!("{tool} -I INPUT -p tcp --dport {PORT} -j ACCEPT"));
                lines.push(format!("{tool} -I INPUT -p udp --dport {PORT} -j ACCEPT"));
                lines.push(format!("{tool} -I INPUT -p udp --dport {MDNS} -j ACCEPT"));
            }
        }
    }
    lines
}

pub fn firewall() -> Check {
    let (status, message, fixable) = match active_firewall() {
        None => match rules_services()[..] {
            [] => (
                Status::Ok,
                "Neither ufw nor firewalld is on, nothing to open.".to_string(),
                false,
            ),
            ref services => (
                Status::Warn,
                format!(
                    "{} is on, and its rules can't be read without admin rights. Make sure they let in TCP and UDP port {PORT} and UDP port {MDNS} (mDNS), for example with the default table and chain names:\n{}",
                    services.join(" and "),
                    rules_lines(services).join("\n")
                ),
                false,
            ),
        },
        Some(Firewall::Firewalld) => {
            let mut blocked = Vec::new();
            let mut error = None;
            let zones = firewalld_zones();
            for zone in &zones {
                match read_zone(zone) {
                    Ok(open) if open.missing().is_empty() => (),
                    Ok(open) => {
                        blocked.push(format!("zone {zone} blocks {}", open.missing().join(", ")))
                    }
                    Err(e) => error = Some(e),
                }
            }
            match (blocked.is_empty(), error) {
                (true, None) => (
                    Status::Ok,
                    format!(
                        "firewalld lets the headset in on port {PORT} and mDNS (zone {}).",
                        zones.join(", ")
                    ),
                    false,
                ),
                (true, Some(e)) => (
                    Status::Warn,
                    format!("Could not read the firewalld rules: {e}"),
                    true,
                ),
                (false, _) => (
                    Status::Fail,
                    format!("firewalld {}.", blocked.join(", ")),
                    true,
                ),
            }
        }
        Some(Firewall::Ufw) => match read_ufw() {
            Ok(open) if open.missing().is_empty() => (
                Status::Ok,
                format!("ufw lets the headset in on port {PORT} and mDNS."),
                false,
            ),
            Ok(open) => (
                Status::Fail,
                format!("ufw blocks {}.", open.missing().join(", ")),
                true,
            ),
            Err(e) => (
                Status::Warn,
                format!("Could not read the ufw rules: {e}"),
                true,
            ),
        },
    };
    Check {
        id: "firewall",
        title: "Firewall",
        status,
        message,
        fix: fixable.then_some(FIX),
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
            for zone in firewalld_zones() {
                let zone = format!("--zone={zone}");
                run(
                    "firewall-cmd",
                    &["--permanent", &zone, &tcp, &udp, "--add-service=mdns"],
                )?;
            }
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
/// that, `lvrr doctor` reports it. Hand-written nftables or iptables rules are left alone.
pub fn first_start(config_dir: &Path) {
    let asked = config_dir.join(ASKED_FILE);
    if asked.exists() {
        return;
    }
    let check = firewall();
    if check.status == Status::Ok || check.fix.is_none() {
        return;
    }
    if let Err(e) = fs::write(&asked, "") {
        eprintln!("lvrr: writing {}: {e}", asked.display());
        return;
    }
    crate::report(
        "Opening the firewall for LVRR",
        &format!("{} This asks for an admin password.", check.message),
    );
    if let Err(e) = fix_firewall() {
        crate::report(
            "LVRR could not open the firewall",
            &format!("{e}. Run `{FIX}` to try again."),
        );
    }
}

fn checks(root: &Path) -> Vec<Check> {
    let config = crate::read_config();
    let session_running = bus::session().is_ok_and(|conn| {
        matches!(server::state(&conn, root), State::Ours)
            && Server::new(&conn)
                .get::<bool>("SessionRunning")
                .unwrap_or(false)
    });
    vec![
        firewall(),
        checks::avahi(&config),
        checks::port(root, &config),
        checks::vulkan(),
        checks::openxr(root, session_running),
        checks::openvr(root, &config),
        checks::steam(root),
        checks::uinput(&config),
        checks::pipewire(),
    ]
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
    let root = match crate::install_root() {
        Ok(root) => root,
        Err(e) => {
            eprintln!("lvrr: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut report = checks(&root);
    if fix {
        let mut fixed = false;
        for c in report
            .iter()
            .filter(|c| c.status != Status::Ok && c.fix.is_some())
        {
            let result = match c.id {
                "firewall" => fix_firewall(),
                "avahi" => checks::fix_avahi(),
                "steam" => checks::fix_steam(&root),
                _ => continue,
            };
            if let Err(e) = result {
                eprintln!("lvrr: {e}");
            }
            fixed = true;
        }
        if fixed {
            report = checks(&root);
        }
    }
    if json {
        println!("{}", to_json(&report));
    } else {
        for c in &report {
            let message = c.message.replace('\n', "\n      ");
            println!("{:4}  {}: {message}", c.status.name(), c.title);
            if let Some(fix) = c.fix {
                println!("      fix: {fix}");
            }
        }
    }
    if report.iter().any(|c| c.status == Status::Fail) {
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
        let trusted = r#"<zone target="ACCEPT"><short>Trusted</short></zone>"#;
        assert!(firewalld_zone_open(trusted).missing().is_empty());
        let drop = r#"<zone target="DROP"><service name="ssh"/></zone>"#;
        assert_eq!(firewalld_zone_open(drop).missing().len(), 3);
    }

    #[test]
    fn firewalld_active_zones() {
        let text = "home (default)\n  interfaces: wlp2s0 enp3s0\ndocker\n  interfaces: docker0\nlibvirt\n  sources: 192.168.122.0/24\n";
        assert_eq!(
            active_zones(text),
            [
                (
                    "home".to_string(),
                    vec!["wlp2s0".to_string(), "enp3s0".to_string()]
                ),
                ("docker".to_string(), vec!["docker0".to_string()]),
                ("libvirt".to_string(), vec![]),
            ]
        );
    }

    #[test]
    fn default_routes() {
        let ipv4 =
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
wlp2s0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
wlp2s0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0
";
        let ipv6 = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000258 00000002 00000000 00000003 enp3s0
00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo
fe800000000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000001 00000000 00000001 wlp2s0
";
        assert_eq!(default_route_interfaces(ipv4, ipv6), ["enp3s0", "wlp2s0"]);
    }

    #[test]
    fn hand_written_rules() {
        let lines = rules_lines(&["nftables", "ip6tables"]);
        assert_eq!(
            lines[1],
            "nft insert rule inet filter input udp dport { 9757, 5353 } accept"
        );
        assert_eq!(lines[4], "ip6tables -I INPUT -p udp --dport 5353 -j ACCEPT");
        assert_eq!(lines.len(), 5);
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
