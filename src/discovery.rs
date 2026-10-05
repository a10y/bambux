//! LAN printer discovery. Bambu printers answer SSDP `M-SEARCH` requests for
//! their own search target on UDP 1990 and 2021. Searching (rather than
//! listening for their periodic `NOTIFY`s on 2021) gets replies on our own
//! socket, so it works even while Bambu Studio holds port 2021.

use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

use anyhow::Result;

const SEARCH_TARGET: &str = "urn:bambulab-com:device:3dprinter:1";
const SSDP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const PORTS: [u16; 2] = [1990, 2021];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPrinter {
    pub serial: String,
    pub host: String,
    pub name: String,
    /// Bambu's internal model code, e.g. `N7`.
    pub model_code: String,
    pub firmware: Option<String>,
    /// `lan` for LAN-only mode, `cloud` otherwise.
    pub connection: Option<String>,
}

impl DiscoveredPrinter {
    pub fn model_name(&self) -> &str {
        model_name(&self.model_code)
    }
}

/// Marketing name for a model code, or the code itself when unknown.
pub fn model_name(code: &str) -> &str {
    match code {
        "BL-P001" => "X1 Carbon",
        "BL-P002" => "X1",
        "C13" => "X1E",
        "C11" => "P1P",
        "C12" => "P1S",
        "N7" => "P2S",
        "N1" => "A1 mini",
        "N2S" => "A1",
        "O1D" => "H2D",
        other => other,
    }
}

/// Search the local network for `wait`, returning each printer once.
pub fn discover(wait: Duration) -> Result<Vec<DiscoveredPrinter>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    socket.set_multicast_ttl_v4(2)?;
    let request = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_GROUP}:1990\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {SEARCH_TARGET}\r\n\r\n"
    );
    let destinations = PORTS.iter().flat_map(|&port| {
        [
            SocketAddr::from((SSDP_GROUP, port)),
            SocketAddr::from((Ipv4Addr::BROADCAST, port)),
        ]
    });
    let mut sent = false;
    for destination in destinations {
        // Some networks reject broadcast or multicast; any one path is enough.
        sent |= socket.send_to(request.as_bytes(), destination).is_ok();
    }
    if !sent {
        anyhow::bail!("could not send a discovery request on this network");
    }

    let mut found = BTreeMap::new();
    let deadline = Instant::now() + wait;
    let mut buffer = [0u8; 4096];
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        socket.set_read_timeout(Some(remaining.max(Duration::from_millis(1))))?;
        let Ok((len, from)) = socket.recv_from(&mut buffer) else {
            break;
        };
        if let Some(printer) = parse_response(&String::from_utf8_lossy(&buffer[..len]), from) {
            found.entry(printer.serial.clone()).or_insert(printer);
        }
    }
    Ok(found.into_values().collect())
}

/// Parse an SSDP response or `NOTIFY` from a Bambu printer.
fn parse_response(text: &str, from: SocketAddr) -> Option<DiscoveredPrinter> {
    let headers: BTreeMap<String, String> = text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    let target = headers.get("st").or_else(|| headers.get("nt"))?;
    if target != SEARCH_TARGET {
        return None;
    }
    let header = |key: &str| headers.get(key).filter(|value| !value.is_empty()).cloned();
    let serial = header("usn")?;
    // `Location` is the bare IP; fall back to the sender's address.
    let host = header("location").unwrap_or_else(|| from.ip().to_string());
    Some(DiscoveredPrinter {
        name: header("devname.bambu.com").unwrap_or_else(|| serial.clone()),
        model_code: header("devmodel.bambu.com").unwrap_or_default(),
        firmware: header("devversion.bambu.com"),
        connection: header("devconnect.bambu.com"),
        serial,
        host,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_p2s_search_response() {
        // Captured from a P2S, serial replaced.
        let response = "HTTP/1.1 200 OK\r\nServer: UPnP/1.0\r\nLocation: 192.168.50.113\r\n\
            ST: urn:bambulab-com:device:3dprinter:1\r\nEXT: \r\nUSN: 22E00000000000\r\n\
            Cache-Control: max-age=1800\r\nDevModel.bambu.com: N7\r\nDevName.bambu.com: 3DP-22E-396\r\n\
            DevConnect.bambu.com: lan\r\nDevBind.bambu.com: free\r\nDevVersion.bambu.com: 01.00.05.00\r\n\r\n";
        let from: SocketAddr = "192.168.50.113:1990".parse().unwrap();
        let printer = parse_response(response, from).unwrap();
        assert_eq!(printer.serial, "22E00000000000");
        assert_eq!(printer.host, "192.168.50.113");
        assert_eq!(printer.name, "3DP-22E-396");
        assert_eq!(printer.model_name(), "P2S");
        assert_eq!(printer.firmware.as_deref(), Some("01.00.05.00"));
        assert_eq!(printer.connection.as_deref(), Some("lan"));
    }

    #[test]
    fn ignores_other_ssdp_devices() {
        let response = "HTTP/1.1 200 OK\r\nST: upnp:rootdevice\r\nUSN: uuid:abc\r\n\r\n";
        assert!(parse_response(response, "10.0.0.2:1900".parse().unwrap()).is_none());
    }
}

