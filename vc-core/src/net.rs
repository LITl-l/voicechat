use anyhow::{anyhow, Result};
use std::net::SocketAddr;

// Packet type constants
pub const PKT_AUDIO: u8 = 0x01;
pub const PKT_JOIN: u8 = 0x02;
pub const PKT_HELLO: u8 = 0x03;
pub const PKT_PEER_LIST: u8 = 0x04;
pub const PKT_KEEPALIVE: u8 = 0x05;
pub const PKT_HELLO_ACK: u8 = 0x06;
pub const PKT_PING: u8 = 0x07;
pub const PKT_PONG: u8 = 0x08;
pub const PKT_KEY_EXCHANGE: u8 = 0x09;

pub const HEADER_LEN: usize = 8;
pub const MAX_PACKET_LEN: usize = 1200; // well under MTU

/// 8-byte packet header.
///
/// The `counter` field carries the per-sender monotonic nonce counter so the
/// receiver can reconstruct the exact XChaCha20 nonce used to encrypt the
/// packet. It is authenticated as part of the AEAD AAD (via `to_bytes()`)
/// so a tampered counter causes decrypt to fail.
#[derive(Clone, Copy, Debug)]
pub struct PacketHeader {
    pub pkt_type: u8,
    pub peer_id: u8,
    pub seq_num: u16,
    pub counter: u32,
}

impl PacketHeader {
    pub fn new(pkt_type: u8, peer_id: u8, seq_num: u16, counter: u32) -> Self {
        Self {
            pkt_type,
            peer_id,
            seq_num,
            counter,
        }
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0] = self.pkt_type;
        buf[1] = self.peer_id;
        buf[2..4].copy_from_slice(&self.seq_num.to_be_bytes());
        buf[4..8].copy_from_slice(&self.counter.to_be_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_LEN {
            return Err(anyhow!("packet too short for header: {} bytes", buf.len()));
        }
        Ok(Self {
            pkt_type: buf[0],
            peer_id: buf[1],
            seq_num: u16::from_be_bytes([buf[2], buf[3]]),
            counter: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        })
    }
}

/// Raw UDP socket wrapper with low-latency settings.
pub struct UdpSocket {
    inner: std::net::UdpSocket,
}

impl UdpSocket {
    pub fn bind(addr: SocketAddr) -> Result<Self> {
        use socket2::{Domain, Protocol, Socket, Type};

        let domain = if addr.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;

        socket.set_reuse_address(true)?;
        socket.set_nonblocking(false)?;

        // Small send/recv buffers to minimize kernel-side latency
        socket.set_send_buffer_size(65536)?;
        socket.set_recv_buffer_size(65536)?;

        // Receive timeout so recv doesn't block forever
        socket.set_read_timeout(Some(std::time::Duration::from_millis(100)))?;

        socket.bind(&addr.into())?;

        Ok(Self {
            inner: socket.into(),
        })
    }

    /// Send a complete packet (header + payload) to a peer.
    pub fn send_to(&self, data: &[u8], addr: SocketAddr) -> Result<usize> {
        Ok(self.inner.send_to(data, addr)?)
    }

    /// Receive a packet. Returns (bytes_read, sender_address).
    /// Returns io::Result so callers can inspect error kind (e.g. WouldBlock).
    pub fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.inner.recv_from(buf)
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.inner.local_addr()?)
    }

    pub fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> Result<()> {
        Ok(self.inner.set_read_timeout(dur)?)
    }
}

/// Build a wire packet: header (cleartext) || encrypted_payload_with_tag.
pub fn build_wire_packet(header: &[u8; HEADER_LEN], encrypted_payload: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(HEADER_LEN + encrypted_payload.len());
    pkt.extend_from_slice(header);
    pkt.extend_from_slice(encrypted_payload);
    pkt
}

/// Parse a wire packet into header bytes and ciphertext portion.
pub fn parse_wire_packet(data: &[u8]) -> Result<([u8; HEADER_LEN], &[u8])> {
    if data.len() < HEADER_LEN {
        return Err(anyhow!(
            "packet too short: {} bytes (need at least {})",
            data.len(),
            HEADER_LEN
        ));
    }
    let mut header = [0u8; HEADER_LEN];
    header.copy_from_slice(&data[..HEADER_LEN]);
    Ok((header, &data[HEADER_LEN..]))
}

// --- Control packet payloads ---

/// JOIN packet payload: just the peer's desired listen port (for NAT info).
#[derive(Clone, Debug)]
pub struct JoinPayload {
    pub listen_port: u16,
}

impl JoinPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        self.listen_port.to_be_bytes().to_vec()
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 2 {
            return Err(anyhow!("JOIN payload too short"));
        }
        Ok(Self {
            listen_port: u16::from_be_bytes([data[0], data[1]]),
        })
    }
}

/// PEER_LIST payload: session_id (8) || count (1) || [peer_id (1) + ip (4/16) + port (2)] ...
#[derive(Clone, Debug)]
pub struct PeerListPayload {
    pub session_id: [u8; 8],
    pub peers: Vec<(u8, SocketAddr)>,
}

impl PeerListPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&self.session_id);
        buf.push(self.peers.len() as u8);
        for &(id, addr) in &self.peers {
            buf.push(id);
            match addr {
                SocketAddr::V4(v4) => {
                    buf.push(4); // IPv4 marker
                    buf.extend_from_slice(&v4.ip().octets());
                    buf.extend_from_slice(&v4.port().to_be_bytes());
                }
                SocketAddr::V6(v6) => {
                    buf.push(6); // IPv6 marker
                    buf.extend_from_slice(&v6.ip().octets());
                    buf.extend_from_slice(&v6.port().to_be_bytes());
                }
            }
        }
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 9 {
            return Err(anyhow!("PEER_LIST payload too short"));
        }
        let mut session_id = [0u8; 8];
        session_id.copy_from_slice(&data[..8]);
        let count = data[8] as usize;
        let mut offset = 9;
        let mut peers = Vec::with_capacity(count);
        for _ in 0..count {
            if offset >= data.len() {
                return Err(anyhow!("PEER_LIST truncated"));
            }
            let id = data[offset];
            offset += 1;
            let ip_ver = data[offset];
            offset += 1;
            let addr = match ip_ver {
                4 => {
                    if offset + 6 > data.len() {
                        return Err(anyhow!("PEER_LIST truncated (IPv4)"));
                    }
                    let ip = std::net::Ipv4Addr::new(
                        data[offset],
                        data[offset + 1],
                        data[offset + 2],
                        data[offset + 3],
                    );
                    let port = u16::from_be_bytes([data[offset + 4], data[offset + 5]]);
                    offset += 6;
                    SocketAddr::V4(std::net::SocketAddrV4::new(ip, port))
                }
                6 => {
                    if offset + 18 > data.len() {
                        return Err(anyhow!("PEER_LIST truncated (IPv6)"));
                    }
                    let mut octets = [0u8; 16];
                    octets.copy_from_slice(&data[offset..offset + 16]);
                    let ip = std::net::Ipv6Addr::from(octets);
                    let port = u16::from_be_bytes([data[offset + 16], data[offset + 17]]);
                    offset += 18;
                    SocketAddr::V6(std::net::SocketAddrV6::new(ip, port, 0, 0))
                }
                _ => return Err(anyhow!("unknown IP version marker: {ip_ver}")),
            };
            peers.push((id, addr));
        }
        Ok(Self { session_id, peers })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_roundtrip() {
        let hdr = PacketHeader::new(PKT_AUDIO, 2, 1234, 56789);
        let bytes = hdr.to_bytes();
        let parsed = PacketHeader::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.pkt_type, PKT_AUDIO);
        assert_eq!(parsed.peer_id, 2);
        assert_eq!(parsed.seq_num, 1234);
        assert_eq!(parsed.counter, 56789);
    }

    #[test]
    fn test_peer_list_roundtrip() {
        let payload = PeerListPayload {
            session_id: [0xAA; 8],
            peers: vec![
                (0, "192.168.1.1:5000".parse().unwrap()),
                (1, "10.0.0.2:6000".parse().unwrap()),
            ],
        };
        let bytes = payload.to_bytes();
        let parsed = PeerListPayload::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.session_id, [0xAA; 8]);
        assert_eq!(parsed.peers.len(), 2);
        assert_eq!(parsed.peers[0].0, 0);
        assert_eq!(parsed.peers[1].0, 1);
    }
}
