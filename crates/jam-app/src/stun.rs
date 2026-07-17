//! Minimal STUN client (RFC 5389 binding request) to discover the public
//! address of the host's UDP port, so the host can tell bandmates what to
//! connect to. Best-effort: failures degrade to a hint, never an error.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

const STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.cloudflare.com:3478",
];

const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_RESPONSE: u16 = 0x0101;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;

/// Asks a public STUN server what address our `socket`'s port appears as
/// from the internet. The same socket the session uses must be passed so the
/// NAT mapping matches.
pub fn discover_public_addr(socket: &UdpSocket) -> Option<SocketAddr> {
    let original_timeout = socket.read_timeout().ok().flatten();
    socket
        .set_read_timeout(Some(Duration::from_millis(1500)))
        .ok()?;
    let result = STUN_SERVERS
        .iter()
        .find_map(|server| try_server(socket, server));
    let _ = socket.set_read_timeout(original_timeout);
    result
}

fn try_server(socket: &UdpSocket, server: &str) -> Option<SocketAddr> {
    let server_addr = server.to_socket_addrs().ok()?.find(|a| a.is_ipv4())?;

    let mut request = [0u8; 20];
    request[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // length 0 — no attributes
    request[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    let txid = transaction_id();
    request[8..20].copy_from_slice(&txid);
    socket.send_to(&request, server_addr).ok()?;

    let mut buf = [0u8; 256];
    // Read until we get our response or time out; other session traffic may
    // interleave, so skip unrelated datagrams (bounded attempts).
    for _ in 0..8 {
        let (len, from) = socket.recv_from(&mut buf).ok()?;
        if from != server_addr || len < 20 {
            continue;
        }
        let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
        if msg_type != BINDING_RESPONSE || buf[8..20] != txid {
            continue;
        }
        return parse_binding_response(&buf[..len]);
    }
    None
}

fn transaction_id() -> [u8; 12] {
    let mut id = [0u8; 12];
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xDEAD_BEEF)
        ^ (std::process::id() as u64).rotate_left(32);
    for chunk in id.chunks_mut(4) {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        chunk.copy_from_slice(&(state >> 32).to_be_bytes()[..chunk.len()]);
    }
    id
}

fn parse_binding_response(msg: &[u8]) -> Option<SocketAddr> {
    let attr_len = u16::from_be_bytes([msg[2], msg[3]]) as usize;
    let mut attrs = msg.get(20..20 + attr_len)?;
    while attrs.len() >= 4 {
        let attr_type = u16::from_be_bytes([attrs[0], attrs[1]]);
        let len = u16::from_be_bytes([attrs[2], attrs[3]]) as usize;
        let value = attrs.get(4..4 + len)?;
        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS => return parse_address(value, true),
            ATTR_MAPPED_ADDRESS => return parse_address(value, false),
            _ => {}
        }
        // Attributes are 4-byte aligned.
        let advance = 4 + len.div_ceil(4) * 4;
        attrs = attrs.get(advance..)?;
    }
    None
}

fn parse_address(value: &[u8], xored: bool) -> Option<SocketAddr> {
    if value.len() < 8 || value[1] != 0x01 {
        return None; // IPv4 only
    }
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    let mut octets = [value[4], value[5], value[6], value[7]];
    if xored {
        port ^= (MAGIC_COOKIE >> 16) as u16;
        let cookie = MAGIC_COOKIE.to_be_bytes();
        for (o, c) in octets.iter_mut().zip(cookie) {
            *o ^= c;
        }
    }
    Some(SocketAddr::from((octets, port)))
}

/// The LAN address this machine would use to reach the internet (no packets
/// are sent: UDP connect only selects a route).
pub fn lan_addr() -> Option<std::net::IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}
