//! Finding network printers: every host on the local subnets with port 9100
//! open, named by SNMP (sysDescr) where the device answers.
//!
//! Nothing is ever sent to port 9100: a non-Zebra printer would print it as text.
//! We only connect and close, then ask SNMP on UDP 161 (read-only, "public").

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::timeout;

const RAW_PORT: u16 = 9100;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(800);
const SNMP_TIMEOUT: Duration = Duration::from_millis(700);
const PARALLEL: usize = 128;
/// Interfaces of containers and VMs, not the office network.
const SKIP_INTERFACES: &[&str] = &["docker", "br-", "veth", "virbr", "vmnet", "tun", "wg"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPrinter {
    pub host: Ipv4Addr,
    /// SNMP system description, e.g. "ZTC ZD420-203dpi ZPL"; None if SNMP didn't answer.
    pub description: Option<String>,
    pub is_zebra: bool,
}

impl NetworkPrinter {
    /// Short name for the printer list, e.g. "ZTC ZD420-203dpi ZPL".
    pub fn label(&self) -> String {
        match &self.description {
            Some(d) => format!("{d} ({})", self.host),
            None => format!("{} (port 9100 open)", self.host),
        }
    }
}

/// Scan the local subnets (at most a /22 each) for port 9100. Zebra printers first.
pub async fn scan() -> Vec<NetworkPrinter> {
    let mut targets: Vec<Ipv4Addr> = local_networks()
        .into_iter()
        .flat_map(|(ip, prefix)| hosts(ip, prefix))
        .collect();
    targets.sort();
    targets.dedup();

    let limit = Arc::new(Semaphore::new(PARALLEL));
    let mut jobs = JoinSet::new();
    for host in targets {
        let permit = limit.clone().acquire_owned().await.expect("semaphore is never closed");
        jobs.spawn(async move {
            let _permit = permit;
            let addr = SocketAddr::from((host, RAW_PORT));
            // Connect and drop at once; never write anything.
            let open = matches!(timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await, Ok(Ok(_)));
            if !open {
                return None;
            }
            let description = snmp_sys_descr(host).await;
            let is_zebra = description.as_deref().is_some_and(looks_like_zebra);
            Some(NetworkPrinter { host, description, is_zebra })
        });
    }

    let mut found = Vec::new();
    while let Some(result) = jobs.join_next().await {
        if let Ok(Some(printer)) = result {
            found.push(printer);
        }
    }
    found.sort_by(|a, b| b.is_zebra.cmp(&a.is_zebra).then(a.host.cmp(&b.host)));
    found
}

fn looks_like_zebra(description: &str) -> bool {
    let d = description.to_ascii_lowercase();
    d.contains("zebra") || d.starts_with("ztc ") || d.contains(" zpl")
}

/// IPv4 address and prefix length of each real network interface.
fn local_networks() -> Vec<(Ipv4Addr, u8)> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs gives a linked list we only read, then free exactly once.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null()
                || ifa.ifa_netmask.is_null()
                || i32::from((*ifa.ifa_addr).sa_family) != libc::AF_INET
            {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
            if SKIP_INTERFACES.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let mask = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
            let prefix = u32::from_be(mask.sin_addr.s_addr).count_ones() as u8;
            if !ip.is_loopback() && !ip.is_link_local() {
                out.push((ip, prefix));
            }
        }
        libc::freeifaddrs(list);
    }
    out
}

/// Other hosts on the subnet; larger subnets are cut down to the /22 around `ip`.
fn hosts(ip: Ipv4Addr, prefix: u8) -> Vec<Ipv4Addr> {
    let prefix = prefix.clamp(22, 30);
    let size = 1u32 << (32 - prefix);
    let network = u32::from(ip) & !(size - 1);
    (1..size - 1)
        .map(|i| Ipv4Addr::from(network | i))
        .filter(|h| *h != ip)
        .collect()
}

/// SNMPv1 GET of sysDescr.0 (1.3.6.1.2.1.1.1.0) with community "public".
const SNMP_GET_SYSDESCR: &[u8] = &[
    0x30, 0x26, // message
    0x02, 0x01, 0x00, // version 1
    0x04, 0x06, b'p', b'u', b'b', b'l', b'i', b'c', // community
    0xA0, 0x19, // GetRequest
    0x02, 0x01, 0x01, // request id
    0x02, 0x01, 0x00, // error status
    0x02, 0x01, 0x00, // error index
    0x30, 0x0E, 0x30, 0x0C, // varbind list, varbind
    0x06, 0x08, 0x2B, 0x06, 0x01, 0x02, 0x01, 0x01, 0x01, 0x00, // OID
    0x05, 0x00, // NULL
];
const SYSDESCR_OID: &[u8] = &[0x06, 0x08, 0x2B, 0x06, 0x01, 0x02, 0x01, 0x01, 0x01, 0x00];

async fn snmp_sys_descr(host: Ipv4Addr) -> Option<String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    socket.send_to(SNMP_GET_SYSDESCR, (host, 161)).await.ok()?;
    let mut buf = [0u8; 1500];
    let (n, from) = timeout(SNMP_TIMEOUT, socket.recv_from(&mut buf)).await.ok()?.ok()?;
    if from.ip() != host {
        return None;
    }
    parse_sys_descr(&buf[..n])
}

/// The OCTET STRING that follows the sysDescr OID in a GetResponse.
fn parse_sys_descr(packet: &[u8]) -> Option<String> {
    let at = packet.windows(SYSDESCR_OID.len()).position(|w| w == SYSDESCR_OID)? + SYSDESCR_OID.len();
    let rest = packet.get(at..)?;
    if *rest.first()? != 0x04 {
        return None;
    }
    let (len, start) = match *rest.get(1)? {
        n if n < 0x80 => (n as usize, 2),
        0x81 => (*rest.get(2)? as usize, 3),
        0x82 => (u16::from_be_bytes([*rest.get(2)?, *rest.get(3)?]) as usize, 4),
        _ => return None,
    };
    let text = String::from_utf8_lossy(rest.get(start..start + len)?);
    let text = text.split(['\r', '\n']).next().unwrap_or("").trim().to_owned();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_hosts() {
        let h = hosts(Ipv4Addr::new(192, 168, 1, 20), 24);
        assert_eq!(h.len(), 253);
        assert_eq!(h[0], Ipv4Addr::new(192, 168, 1, 1));
        assert!(!h.contains(&Ipv4Addr::new(192, 168, 1, 20)));
        // A /16 is cut down to the /22 around us.
        let big = hosts(Ipv4Addr::new(10, 0, 5, 9), 16);
        assert_eq!((big[0], big.len()), (Ipv4Addr::new(10, 0, 4, 1), 1021));
    }

    #[test]
    fn parses_snmp_reply() {
        let descr = b"ZTC ZD420-203dpi ZPL";
        let mut reply = vec![0x30, 0x40, 0x02, 0x01, 0x00, 0x04, 0x06];
        reply.extend_from_slice(b"public");
        reply.extend_from_slice(&[0xA2, 0x30, 0x02, 0x01, 0x01, 0x02, 0x01, 0x00, 0x02, 0x01, 0x00, 0x30, 0x25, 0x30, 0x23]);
        reply.extend_from_slice(SYSDESCR_OID);
        reply.extend_from_slice(&[0x04, descr.len() as u8]);
        reply.extend_from_slice(descr);
        assert_eq!(parse_sys_descr(&reply).as_deref(), Some("ZTC ZD420-203dpi ZPL"));
        assert!(looks_like_zebra("ZTC ZD420-203dpi ZPL"));
        assert!(!looks_like_zebra("HP ETHERNET MULTI-ENVIRONMENT"));
    }
}
