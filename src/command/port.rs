use crate::cli::PortCommands;
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::Path,
};

const PROC_ROOT: &str = "/proc";
const PROC_NET_ROOT: &str = "/proc/net";
const PORT_REPORT_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortTransport {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortFamily {
    V4,
    V6,
}

#[derive(Debug, Clone, Copy)]
struct PortProtocol {
    label: &'static str,
    proc_name: &'static str,
    transport: PortTransport,
    family: PortFamily,
}

const PORT_PROTOCOLS: [PortProtocol; 4] = [
    PortProtocol {
        label: "tcp",
        proc_name: "tcp",
        transport: PortTransport::Tcp,
        family: PortFamily::V4,
    },
    PortProtocol {
        label: "tcp6",
        proc_name: "tcp6",
        transport: PortTransport::Tcp,
        family: PortFamily::V6,
    },
    PortProtocol {
        label: "udp",
        proc_name: "udp",
        transport: PortTransport::Udp,
        family: PortFamily::V4,
    },
    PortProtocol {
        label: "udp6",
        proc_name: "udp6",
        transport: PortTransport::Udp,
        family: PortFamily::V6,
    },
];

#[derive(Debug, Clone)]
struct PortLsOptions {
    json: bool,
    all: bool,
    ports: BTreeSet<u16>,
    pids: BTreeSet<u32>,
    protocol_filter_requested: bool,
    include_tcp: bool,
    include_udp: bool,
}

#[derive(Debug, Clone)]
struct ProcSocketRow {
    protocol: PortProtocol,
    local: SocketEndpoint,
    remote: SocketEndpoint,
    state_hex: String,
    inode: u64,
}

#[derive(Debug, Clone)]
struct SocketEndpoint {
    ip: IpAddr,
    port: u16,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct PortOwner {
    pid: u32,
    process: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct PortRow {
    proto: String,
    address: String,
    port: u16,
    local: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer: Option<String>,
    state: String,
    inode: u64,
    pids: Vec<u32>,
    processes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    owners: Vec<PortOwner>,
}

#[derive(Debug, Serialize)]
struct PortReport {
    schema_version: u8,
    platform: &'static str,
    listen_only: bool,
    filters: PortFilterSummary,
    rows: Vec<PortRow>,
    rows_without_visible_owner: usize,
    rows_hidden_by_pid_filter_due_to_owner_visibility: usize,
}

#[derive(Debug, Serialize)]
struct PortFilterSummary {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ports: Vec<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pids: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    protocols: Vec<&'static str>,
}

#[derive(Debug)]
struct RenderRow {
    proto: String,
    address: String,
    port: String,
    peer: Option<String>,
    state: String,
    pid: String,
    process: String,
}

pub fn run(cmd: PortCommands) -> Result<i32> {
    match cmd {
        PortCommands::Ls {
            json,
            all,
            port,
            pid,
            tcp,
            udp,
        } => run_ls(PortLsOptions {
            json,
            all,
            ports: port.into_iter().collect(),
            pids: pid.into_iter().collect(),
            protocol_filter_requested: tcp || udp,
            include_tcp: tcp || !udp,
            include_udp: udp || !tcp,
        }),
    }
}

fn run_ls(options: PortLsOptions) -> Result<i32> {
    let report = collect_port_report(&options)?;
    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize port output")?
        );
    } else {
        print!("{}", render_port_report(&report));
    }
    Ok(0)
}

fn collect_port_report(options: &PortLsOptions) -> Result<PortReport> {
    #[cfg(target_os = "linux")]
    {
        collect_port_report_linux(options)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = options;
        bail!("`za port` is currently only supported on Linux with /proc");
    }
}

#[cfg(target_os = "linux")]
fn collect_port_report_linux(options: &PortLsOptions) -> Result<PortReport> {
    let proc_root = Path::new(PROC_ROOT);
    if !proc_root.exists() {
        bail!("`za port` is only supported on Linux with /proc");
    }

    let owners_by_inode = collect_socket_owners(proc_root, &options.pids)?;
    let mut rows = Vec::new();
    let mut rows_without_visible_owner = 0usize;
    let mut rows_hidden_by_pid_filter_due_to_owner_visibility = 0usize;

    for protocol in selected_protocols(options) {
        let path = Path::new(PROC_NET_ROOT).join(protocol.proc_name);
        if !path.exists() {
            continue;
        }
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) => {
                return Err(err).with_context(|| format!("read {}", path.display()));
            }
        };
        for line in raw.lines().skip(1) {
            let Some(socket) = parse_proc_net_line(line, protocol) else {
                continue;
            };
            if !options.all && !is_default_port_listing_candidate(&socket) {
                continue;
            }
            if !options.ports.is_empty() && !options.ports.contains(&socket.local.port) {
                continue;
            }

            let owners = owners_by_inode
                .get(&socket.inode)
                .cloned()
                .unwrap_or_default();
            if !options.pids.is_empty() {
                if owners.is_empty() {
                    rows_hidden_by_pid_filter_due_to_owner_visibility += 1;
                    continue;
                }
                if !owners.iter().any(|owner| options.pids.contains(&owner.pid)) {
                    continue;
                }
            }
            if owners.is_empty() {
                rows_without_visible_owner += 1;
            }
            rows.push(build_port_row(&socket, owners, options.all));
        }
    }

    rows.sort_by(|a, b| {
        a.port
            .cmp(&b.port)
            .then_with(|| a.proto.cmp(&b.proto))
            .then_with(|| a.address.cmp(&b.address))
            .then_with(|| a.state.cmp(&b.state))
            .then_with(|| a.pid_key().cmp(&b.pid_key()))
    });

    Ok(PortReport {
        schema_version: PORT_REPORT_SCHEMA_VERSION,
        platform: "linux",
        listen_only: !options.all,
        filters: PortFilterSummary {
            ports: options.ports.iter().copied().collect(),
            pids: options.pids.iter().copied().collect(),
            protocols: protocol_filter_labels(options),
        },
        rows,
        rows_without_visible_owner,
        rows_hidden_by_pid_filter_due_to_owner_visibility,
    })
}

fn selected_protocols(options: &PortLsOptions) -> Vec<PortProtocol> {
    PORT_PROTOCOLS
        .iter()
        .copied()
        .filter(|protocol| match protocol.transport {
            PortTransport::Tcp => options.include_tcp,
            PortTransport::Udp => options.include_udp,
        })
        .collect()
}

fn protocol_filter_labels(options: &PortLsOptions) -> Vec<&'static str> {
    if !options.protocol_filter_requested {
        return Vec::new();
    }
    let mut out = Vec::new();
    if options.include_tcp {
        out.push("tcp");
    }
    if options.include_udp {
        out.push("udp");
    }
    out
}

#[cfg(target_os = "linux")]
fn collect_socket_owners(
    proc_root: &Path,
    pid_filters: &BTreeSet<u32>,
) -> Result<HashMap<u64, Vec<PortOwner>>> {
    let mut owners_by_inode: HashMap<u64, BTreeMap<u32, String>> = HashMap::new();
    for entry in fs::read_dir(proc_root).context("read /proc")? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(pid) = parse_pid_dir_name(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        if !pid_filters.is_empty() && !pid_filters.contains(&pid) {
            continue;
        }

        let proc_dir = entry.path();
        let process = read_process_name(&proc_dir).unwrap_or_else(|| pid.to_string());
        let fd_dir = proc_dir.join("fd");
        let fd_entries = match fs::read_dir(&fd_dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for fd_entry in fd_entries {
            let fd_entry = match fd_entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let target = match fs::read_link(fd_entry.path()) {
                Ok(target) => target,
                Err(_) => continue,
            };
            let Some(inode) = parse_socket_inode(&target.to_string_lossy()) else {
                continue;
            };
            owners_by_inode
                .entry(inode)
                .or_default()
                .entry(pid)
                .or_insert_with(|| process.clone());
        }
    }

    Ok(owners_by_inode
        .into_iter()
        .map(|(inode, owners)| {
            let owners = owners
                .into_iter()
                .map(|(pid, process)| PortOwner { pid, process })
                .collect::<Vec<_>>();
            (inode, owners)
        })
        .collect())
}

#[cfg(target_os = "linux")]
fn read_process_name(proc_dir: &Path) -> Option<String> {
    let comm_path = proc_dir.join("comm");
    if let Ok(raw) = fs::read_to_string(&comm_path) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let cmdline_path = proc_dir.join("cmdline");
    let raw = fs::read(cmdline_path).ok()?;
    let first = raw.split(|b| *b == 0).find(|part| !part.is_empty())?;
    let first = String::from_utf8_lossy(first);
    let name = Path::new(first.as_ref())
        .file_name()
        .and_then(|part| part.to_str())
        .unwrap_or(first.as_ref())
        .trim();
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

fn parse_pid_dir_name(name: &str) -> Option<u32> {
    if name.chars().all(|c| c.is_ascii_digit()) {
        return name.parse::<u32>().ok();
    }
    None
}

fn parse_socket_inode(link_target: &str) -> Option<u64> {
    let value = link_target.strip_prefix("socket:[")?.strip_suffix(']')?;
    value.parse::<u64>().ok()
}

fn parse_proc_net_line(raw: &str, protocol: PortProtocol) -> Option<ProcSocketRow> {
    let fields = raw.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 10 {
        return None;
    }
    Some(ProcSocketRow {
        protocol,
        local: parse_proc_net_socket_endpoint(fields.get(1)?, protocol.family).ok()?,
        remote: parse_proc_net_socket_endpoint(fields.get(2)?, protocol.family).ok()?,
        state_hex: fields.get(3)?.to_ascii_uppercase(),
        inode: fields.get(9)?.parse::<u64>().ok()?,
    })
}

fn parse_proc_net_socket_endpoint(raw: &str, family: PortFamily) -> Result<SocketEndpoint> {
    let (ip_hex, port_hex) = raw
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid socket endpoint `{raw}`"))?;
    let ip = match family {
        PortFamily::V4 => IpAddr::V4(parse_ipv4_hex(ip_hex)?),
        PortFamily::V6 => IpAddr::V6(parse_ipv6_hex(ip_hex)?),
    };
    let port =
        u16::from_str_radix(port_hex, 16).with_context(|| format!("parse port from `{raw}`"))?;
    Ok(SocketEndpoint { ip, port })
}

fn parse_ipv4_hex(raw: &str) -> Result<Ipv4Addr> {
    if raw.len() != 8 {
        bail!("invalid IPv4 hex address `{raw}`");
    }
    let value = u32::from_str_radix(raw, 16).with_context(|| format!("parse IPv4 `{raw}`"))?;
    Ok(Ipv4Addr::from(value.to_le_bytes()))
}

fn parse_ipv6_hex(raw: &str) -> Result<Ipv6Addr> {
    if raw.len() != 32 {
        bail!("invalid IPv6 hex address `{raw}`");
    }
    let mut bytes = [0_u8; 16];
    for (index, chunk) in raw.as_bytes().chunks(8).enumerate() {
        let chunk = std::str::from_utf8(chunk).ok().unwrap_or_default();
        let value =
            u32::from_str_radix(chunk, 16).with_context(|| format!("parse IPv6 `{raw}`"))?;
        bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    Ok(Ipv6Addr::from(bytes))
}

fn is_default_port_listing_candidate(socket: &ProcSocketRow) -> bool {
    match socket.protocol.transport {
        PortTransport::Tcp => socket.state_hex == "0A",
        PortTransport::Udp => socket.remote.is_unspecified(),
    }
}

fn build_port_row(socket: &ProcSocketRow, owners: Vec<PortOwner>, include_peer: bool) -> PortRow {
    let processes = owners
        .iter()
        .map(|owner| owner.process.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let pids = owners.iter().map(|owner| owner.pid).collect::<Vec<_>>();

    PortRow {
        proto: socket.protocol.label.to_string(),
        address: display_bind_address(socket.local.ip),
        port: socket.local.port,
        local: format_socket_endpoint(&socket.local),
        peer: (include_peer && !socket.remote.is_unspecified())
            .then(|| format_socket_endpoint(&socket.remote)),
        state: socket_state_label(socket.protocol.transport, &socket.state_hex).to_string(),
        inode: socket.inode,
        pids,
        processes,
        owners,
    }
}

fn socket_state_label(transport: PortTransport, state_hex: &str) -> &'static str {
    match transport {
        PortTransport::Tcp => match state_hex {
            "01" => "ESTABLISHED",
            "02" => "SYN-SENT",
            "03" => "SYN-RECV",
            "04" => "FIN-WAIT-1",
            "05" => "FIN-WAIT-2",
            "06" => "TIME-WAIT",
            "07" => "CLOSE",
            "08" => "CLOSE-WAIT",
            "09" => "LAST-ACK",
            "0A" => "LISTEN",
            "0B" => "CLOSING",
            "0C" => "NEW-SYN-RECV",
            _ => "UNKNOWN",
        },
        PortTransport::Udp => match state_hex {
            "01" => "ESTABLISHED",
            "07" => "UNCONN",
            "0A" => "LISTEN",
            _ => "UNKNOWN",
        },
    }
}

fn display_bind_address(ip: IpAddr) -> String {
    if ip.is_unspecified() {
        "*".to_string()
    } else {
        ip.to_string()
    }
}

fn format_socket_endpoint(endpoint: &SocketEndpoint) -> String {
    let host = display_bind_address(endpoint.ip);
    match endpoint.ip {
        IpAddr::V4(_) => format!("{host}:{}", endpoint.port),
        IpAddr::V6(_) if host == "*" => format!("*:{}", endpoint.port),
        IpAddr::V6(_) => format!("[{host}]:{}", endpoint.port),
    }
}

fn render_port_report(report: &PortReport) -> String {
    if report.rows.is_empty() {
        let mut out = String::new();
        if report.listen_only {
            out.push_str("No listening or bound ports matched.\n");
        } else {
            out.push_str("No local sockets matched.\n");
        }
        let filter_line = render_filter_summary(report);
        if let Some(filter_line) = filter_line {
            out.push_str(&filter_line);
            out.push('\n');
        }
        if report.rows_hidden_by_pid_filter_due_to_owner_visibility > 0 {
            out.push_str(&render_pid_filter_visibility_hint(report));
            out.push('\n');
        }
        return out;
    }

    let rows = report
        .rows
        .iter()
        .map(|row| RenderRow {
            proto: row.proto.clone(),
            address: row.address.clone(),
            port: row.port.to_string(),
            peer: row.peer.clone(),
            state: row.state.clone(),
            pid: render_pids(&row.pids),
            process: render_processes(&row.processes),
        })
        .collect::<Vec<_>>();

    let proto_width = column_width("PROTO", rows.iter().map(|row| row.proto.as_str()));
    let address_width = column_width("ADDRESS", rows.iter().map(|row| row.address.as_str()));
    let port_width = column_width("PORT", rows.iter().map(|row| row.port.as_str()));
    let state_width = column_width("STATE", rows.iter().map(|row| row.state.as_str()));
    let pid_width = column_width("PID", rows.iter().map(|row| row.pid.as_str()));
    let process_width = column_width("PROCESS", rows.iter().map(|row| row.process.as_str()));
    let peer_width = report
        .rows
        .iter()
        .filter_map(|row| row.peer.as_deref())
        .collect::<Vec<_>>();

    let mut lines = Vec::with_capacity(rows.len() + 4);
    if peer_width.is_empty() {
        lines.push(format!(
            "{:<proto_width$} {:<address_width$} {:>port_width$} {:<state_width$} {:<pid_width$} {:<process_width$}",
            "PROTO", "ADDRESS", "PORT", "STATE", "PID", "PROCESS"
        ));
        for row in rows {
            lines.push(format!(
                "{:<proto_width$} {:<address_width$} {:>port_width$} {:<state_width$} {:<pid_width$} {:<process_width$}",
                row.proto, row.address, row.port, row.state, row.pid, row.process
            ));
        }
    } else {
        let peer_width = column_width("PEER", peer_width);
        lines.push(format!(
            "{:<proto_width$} {:<address_width$} {:>port_width$} {:<peer_width$} {:<state_width$} {:<pid_width$} {:<process_width$}",
            "PROTO", "ADDRESS", "PORT", "PEER", "STATE", "PID", "PROCESS"
        ));
        for row in rows {
            lines.push(format!(
                "{:<proto_width$} {:<address_width$} {:>port_width$} {:<peer_width$} {:<state_width$} {:<pid_width$} {:<process_width$}",
                row.proto,
                row.address,
                row.port,
                row.peer.as_deref().unwrap_or("-"),
                row.state,
                row.pid,
                row.process
            ));
        }
    }

    lines.push(String::new());
    lines.push(format!(
        "Showing {} row(s) on Linux{}",
        report.rows.len(),
        if report.listen_only {
            "; default view hides connected sockets, pass `--all` to include them"
        } else {
            ""
        }
    ));
    if let Some(filter_line) = render_filter_summary(report) {
        lines.push(filter_line);
    }
    if report.rows_hidden_by_pid_filter_due_to_owner_visibility > 0 {
        lines.push(render_pid_filter_visibility_hint(report));
    }
    if report.rows_without_visible_owner > 0 {
        lines.push(format!(
            "Owner visibility limited for {} row(s); restricted /proc, hidepid, or sandboxing may hide PID/process names.",
            report.rows_without_visible_owner
        ));
    }

    lines.join("\n") + "\n"
}

fn render_filter_summary(report: &PortReport) -> Option<String> {
    let mut parts = Vec::new();
    if !report.filters.protocols.is_empty() {
        parts.push(format!("proto={}", report.filters.protocols.join(",")));
    }
    if !report.filters.ports.is_empty() {
        parts.push(format!(
            "port={}",
            report
                .filters
                .ports
                .iter()
                .map(|port| port.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if !report.filters.pids.is_empty() {
        parts.push(format!(
            "pid={}",
            report
                .filters
                .pids
                .iter()
                .map(|pid| pid.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("Filters: {}", parts.join("  ")))
}

fn render_pid_filter_visibility_hint(report: &PortReport) -> String {
    format!(
        "PID filter visibility limited for {} candidate row(s); restricted /proc, hidepid, or sandboxing may hide matching owners.",
        report.rows_hidden_by_pid_filter_due_to_owner_visibility
    )
}

fn render_pids(pids: &[u32]) -> String {
    if pids.is_empty() {
        "-".to_string()
    } else {
        pids.iter()
            .map(|pid| pid.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn render_processes(processes: &[String]) -> String {
    if processes.is_empty() {
        "-".to_string()
    } else {
        processes.join(",")
    }
}

fn column_width<'a, I>(header: &str, values: I) -> usize
where
    I: IntoIterator<Item = &'a str>,
{
    values
        .into_iter()
        .fold(header.chars().count(), |width, value| {
            width.max(value.chars().count())
        })
}

impl SocketEndpoint {
    fn is_unspecified(&self) -> bool {
        self.ip.is_unspecified() && self.port == 0
    }
}

impl PortRow {
    fn pid_key(&self) -> String {
        render_pids(&self.pids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::net::{Ipv4Addr, TcpListener};

    #[test]
    fn parse_ipv4_socket_endpoint_decodes_loopback() {
        let endpoint =
            parse_proc_net_socket_endpoint("0100007F:1F90", PortFamily::V4).expect("endpoint");
        assert_eq!(endpoint.ip, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(endpoint.port, 8080);
    }

    #[test]
    fn parse_ipv6_socket_endpoint_decodes_loopback() {
        let endpoint =
            parse_proc_net_socket_endpoint("00000000000000000000000001000000:01BB", PortFamily::V6)
                .expect("endpoint");
        assert_eq!(endpoint.ip, IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert_eq!(endpoint.port, 443);
    }

    #[test]
    fn parse_proc_net_line_extracts_core_fields() {
        let row = parse_proc_net_line(
            "0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 12345 1 0000000000000000 100 0 0 10 0",
            PORT_PROTOCOLS[0],
        )
        .expect("row");
        assert_eq!(row.local.port, 8080);
        assert_eq!(row.remote.port, 0);
        assert_eq!(row.state_hex, "0A");
        assert_eq!(row.inode, 12345);
    }

    #[test]
    fn render_port_report_shows_filters_and_owner_visibility_hint() {
        let report = PortReport {
            schema_version: PORT_REPORT_SCHEMA_VERSION,
            platform: "linux",
            listen_only: true,
            filters: PortFilterSummary {
                ports: vec![8080],
                pids: vec![123],
                protocols: vec!["tcp"],
            },
            rows: vec![PortRow {
                proto: "tcp".to_string(),
                address: "*".to_string(),
                port: 8080,
                local: "*:8080".to_string(),
                peer: None,
                state: "LISTEN".to_string(),
                inode: 12345,
                pids: vec![123],
                processes: vec!["python".to_string()],
                owners: vec![PortOwner {
                    pid: 123,
                    process: "python".to_string(),
                }],
            }],
            rows_without_visible_owner: 1,
            rows_hidden_by_pid_filter_due_to_owner_visibility: 0,
        };

        let rendered = render_port_report(&report);
        assert!(rendered.contains("PROTO"));
        assert!(rendered.contains("tcp"));
        assert!(rendered.contains("8080"));
        assert!(rendered.contains("Filters: proto=tcp  port=8080  pid=123"));
        assert!(rendered.contains("Owner visibility limited for 1 row(s)"));
    }

    #[test]
    fn render_port_report_surfaces_pid_filter_visibility_limit_when_empty() {
        let report = PortReport {
            schema_version: PORT_REPORT_SCHEMA_VERSION,
            platform: "linux",
            listen_only: true,
            filters: PortFilterSummary {
                ports: Vec::new(),
                pids: vec![123],
                protocols: Vec::new(),
            },
            rows: Vec::new(),
            rows_without_visible_owner: 0,
            rows_hidden_by_pid_filter_due_to_owner_visibility: 2,
        };

        let rendered = render_port_report(&report);
        assert!(rendered.contains("No listening or bound ports matched."));
        assert!(rendered.contains("Filters: pid=123"));
        assert!(rendered.contains("PID filter visibility limited for 2 candidate row(s)"));
    }

    #[test]
    fn build_port_row_omits_unspecified_peer_in_all_view() {
        let socket = ProcSocketRow {
            protocol: PORT_PROTOCOLS[0],
            local: SocketEndpoint {
                ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                port: 8080,
            },
            remote: SocketEndpoint {
                ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                port: 0,
            },
            state_hex: "0A".to_string(),
            inode: 12345,
        };

        let row = build_port_row(&socket, Vec::new(), true);
        assert_eq!(row.peer, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn collect_port_report_discovers_current_process_listener_owner() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind listener");
        let port = listener.local_addr().expect("listener addr").port();
        let pid = std::process::id();

        let report = collect_port_report(&PortLsOptions {
            json: false,
            all: false,
            ports: [port].into_iter().collect(),
            pids: [pid].into_iter().collect(),
            protocol_filter_requested: true,
            include_tcp: true,
            include_udp: false,
        })
        .expect("collect port report");

        assert!(report.rows.iter().any(|row| {
            row.proto == "tcp"
                && row.port == port
                && row.pids.contains(&pid)
                && row.processes.iter().any(|name| !name.is_empty())
        }));
        drop(listener);
    }
}
