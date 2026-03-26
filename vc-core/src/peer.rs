use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use crate::codec;
use crate::crypto::{CryptoContext, ReplayFilter};
use crate::jitter::JitterBuffer;
use crate::latency::LatencyTracker;
use crate::net::*;

pub const MAX_PEERS: usize = 5;
const PEER_TIMEOUT_SECS: f64 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PeerState {
    /// We know about this peer but haven't exchanged HELLO yet.
    Discovered,
    /// HELLO sent, awaiting HELLO_ACK.
    HelloSent,
    /// Bidirectional connectivity confirmed.
    Connected,
    /// Peer timed out (no packets for PEER_TIMEOUT_SECS).
    Disconnected,
}

/// State for a single remote peer.
pub struct RemotePeer {
    pub id: u8,
    pub addr: SocketAddr,
    pub state: PeerState,
    pub jitter_buffer: JitterBuffer,
    pub decoder: codec::Decoder,
    pub replay_filter: ReplayFilter,
    pub last_seen: Instant,
    /// Sequence number for outbound packets to this peer.
    pub send_seq: u16,
    /// Sample timestamp counter.
    pub send_timestamp: u32,
    /// Latency measurement tracker.
    pub latency_tracker: LatencyTracker,
    /// Per-peer crypto context derived from X25519 key exchange.
    pub peer_crypto: Option<CryptoContext>,
    /// Whether we have sent our KEY_EXCHANGE to this peer.
    pub kx_sent: bool,
    /// Whether we have received this peer's KEY_EXCHANGE (and derived key).
    pub kx_received: bool,
}

impl RemotePeer {
    pub fn new(id: u8, addr: SocketAddr) -> Result<Self> {
        Ok(Self {
            id,
            addr,
            state: PeerState::Discovered,
            jitter_buffer: JitterBuffer::new(),
            decoder: codec::Decoder::new()?,
            replay_filter: ReplayFilter::new(),
            last_seen: Instant::now(),
            send_seq: 0,
            send_timestamp: 0,
            latency_tracker: LatencyTracker::new(),
            peer_crypto: None,
            kx_sent: false,
            kx_received: false,
        })
    }

    pub fn is_connected(&self) -> bool {
        self.state == PeerState::Connected
    }

    pub fn is_timed_out(&self) -> bool {
        self.last_seen.elapsed().as_secs_f64() > PEER_TIMEOUT_SECS
    }

    pub fn touch(&mut self) {
        self.last_seen = Instant::now();
    }

    pub fn next_seq(&mut self) -> u16 {
        let seq = self.send_seq;
        self.send_seq = self.send_seq.wrapping_add(1);
        seq
    }

    pub fn advance_timestamp(&mut self) -> u32 {
        let ts = self.send_timestamp;
        self.send_timestamp = self
            .send_timestamp
            .wrapping_add(codec::FRAME_SAMPLES as u32);
        ts
    }
}

/// Manages all peer connections for this node.
pub struct PeerManager {
    /// Our assigned peer ID.
    pub local_id: u8,
    /// Session identifier (shared among all peers).
    pub session_id: [u8; 8],
    /// Remote peers indexed by peer_id.
    pub peers: HashMap<u8, RemotePeer>,
    /// Map from socket address to peer_id for fast lookup on recv.
    pub addr_to_id: HashMap<SocketAddr, u8>,
    /// Whether we are the rendezvous (host) peer.
    pub is_host: bool,
    /// Next peer ID to assign (host only).
    next_peer_id: u8,
}

impl PeerManager {
    /// Create a new PeerManager as the host (rendezvous).
    pub fn new_host(session_id: [u8; 8]) -> Self {
        Self {
            local_id: 0,
            session_id,
            peers: HashMap::new(),
            addr_to_id: HashMap::new(),
            is_host: true,
            next_peer_id: 1, // host is 0
        }
    }

    /// Create a new PeerManager as a joining peer (ID assigned by host).
    pub fn new_joiner() -> Self {
        Self {
            local_id: 0, // will be assigned later
            session_id: [0; 8],
            peers: HashMap::new(),
            addr_to_id: HashMap::new(),
            is_host: false,
            next_peer_id: 0,
        }
    }

    /// Assign a peer ID and session_id after receiving PEER_LIST from host.
    pub fn set_identity(&mut self, local_id: u8, session_id: [u8; 8]) {
        self.local_id = local_id;
        self.session_id = session_id;
    }

    /// Add or update a remote peer.
    pub fn add_peer(&mut self, id: u8, addr: SocketAddr) -> Result<()> {
        if id == self.local_id {
            return Ok(()); // Don't add ourselves
        }
        if self.peers.len() >= MAX_PEERS - 1 {
            return Err(anyhow!("max peers reached"));
        }
        if let std::collections::hash_map::Entry::Vacant(e) = self.peers.entry(id) {
            log::info!("Adding peer {id} at {addr}");
            e.insert(RemotePeer::new(id, addr)?);
            self.addr_to_id.insert(addr, id);
        } else {
            // Update address if changed
            let peer = self.peers.get_mut(&id).unwrap();
            if peer.addr != addr {
                self.addr_to_id.remove(&peer.addr);
                peer.addr = addr;
                self.addr_to_id.insert(addr, id);
            }
        }
        Ok(())
    }

    /// Allocate a new peer ID (host only).
    pub fn allocate_peer_id(&mut self) -> Result<u8> {
        if !self.is_host {
            return Err(anyhow!("only host can allocate peer IDs"));
        }
        if self.next_peer_id as usize >= MAX_PEERS {
            return Err(anyhow!("no more peer IDs available"));
        }
        let id = self.next_peer_id;
        self.next_peer_id += 1;
        Ok(id)
    }

    /// Look up peer by address.
    pub fn peer_by_addr(&self, addr: &SocketAddr) -> Option<u8> {
        self.addr_to_id.get(addr).copied()
    }

    /// Get mutable reference to a peer.
    pub fn get_peer_mut(&mut self, id: u8) -> Option<&mut RemotePeer> {
        self.peers.get_mut(&id)
    }

    /// Get reference to a peer.
    pub fn get_peer(&self, id: u8) -> Option<&RemotePeer> {
        self.peers.get(&id)
    }

    /// Get all connected peer addresses.
    pub fn connected_peers(&self) -> Vec<(u8, SocketAddr)> {
        self.peers
            .values()
            .filter(|p| p.is_connected())
            .map(|p| (p.id, p.addr))
            .collect()
    }

    /// Get all known peer addresses (including non-connected).
    pub fn all_peer_addrs(&self) -> Vec<(u8, SocketAddr)> {
        self.peers.values().map(|p| (p.id, p.addr)).collect()
    }

    /// Build a PEER_LIST payload containing all known peers plus ourself.
    pub fn build_peer_list(&self, local_addr: SocketAddr) -> PeerListPayload {
        let mut peers = vec![(self.local_id, local_addr)];
        for p in self.peers.values() {
            peers.push((p.id, p.addr));
        }
        PeerListPayload {
            session_id: self.session_id,
            peers,
        }
    }

    /// Check for timed-out peers and mark them disconnected.
    pub fn check_timeouts(&mut self) {
        for peer in self.peers.values_mut() {
            if peer.state == PeerState::Connected && peer.is_timed_out() {
                log::warn!("Peer {} timed out", peer.id);
                peer.state = PeerState::Disconnected;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_manager_host() {
        let mut pm = PeerManager::new_host([0xAA; 8]);
        assert_eq!(pm.local_id, 0);
        assert!(pm.is_host);

        let id1 = pm.allocate_peer_id().unwrap();
        assert_eq!(id1, 1);

        pm.add_peer(id1, "127.0.0.1:5001".parse().unwrap()).unwrap();
        assert_eq!(pm.peers.len(), 1);
        assert_eq!(pm.peer_by_addr(&"127.0.0.1:5001".parse().unwrap()), Some(1));
    }

    #[test]
    fn test_max_peers() {
        let mut pm = PeerManager::new_host([0; 8]);
        for i in 1..5u8 {
            let addr: SocketAddr = format!("127.0.0.1:{}", 5000 + i as u16).parse().unwrap();
            pm.add_peer(i, addr).unwrap();
        }
        // 5th peer should fail (we already have 4 remotes + ourself = 5)
        let addr: SocketAddr = "127.0.0.1:5005".parse().unwrap();
        assert!(pm.add_peer(5, addr).is_err());
    }
}
