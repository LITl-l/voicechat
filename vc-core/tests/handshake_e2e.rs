//! End-to-end handshake regression test.
//!
//! Exercises the JOIN → PEER_LIST → HELLO → HELLO_ACK handshake by running
//! the exact same crypto and wire-protocol logic as `Session::run()`, using
//! two real UDP sockets on 127.0.0.1. This test does NOT open audio devices,
//! so it can run in CI environments.
//!
//! If this test fails, the wire protocol or crypto is broken.

use std::net::SocketAddr;
use std::time::Duration;

use vc_core::crypto::{derive_key_from_passphrase, CryptoContext};
use vc_core::net::{
    build_wire_packet, parse_wire_packet, JoinPayload, PacketHeader, PeerListPayload, UdpSocket,
    HEADER_LEN, MAX_PACKET_LEN, PKT_AUDIO, PKT_HELLO, PKT_HELLO_ACK, PKT_JOIN, PKT_PEER_LIST,
};
use vc_core::peer::PeerManager;

fn recv_with_retry(sock: &UdpSocket, buf: &mut [u8]) -> (usize, SocketAddr) {
    for _ in 0..50 {
        match sock.recv_from(buf) {
            Ok(v) => return v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("recv failed: {e}"),
        }
    }
    panic!("recv timed out");
}

#[test]
fn full_handshake_psk() {
    let passphrase = "secret-passphrase-for-test";
    let key = derive_key_from_passphrase(passphrase).unwrap();
    let host_session_id: [u8; 8] = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22];

    let host_sock = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let joiner_sock = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let host_addr = host_sock.local_addr().unwrap();
    let joiner_addr = joiner_sock.local_addr().unwrap();

    host_sock
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    joiner_sock
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();

    let mut host_pm = PeerManager::new_host(host_session_id);
    let mut joiner_pm = PeerManager::new_joiner();

    // ========================================================================
    // Step 1: Joiner sends JOIN
    // Uses fresh zero-session ctx with counter=0 — what host expects to
    // decrypt with before any session_id has been exchanged.
    // ========================================================================
    {
        let join = JoinPayload {
            listen_port: joiner_addr.port(),
        };
        let header = PacketHeader::new(PKT_JOIN, 0, 0, 0);
        let header_bytes = header.to_bytes();
        let psk_ctx = CryptoContext::new(&key, [0; 8]);
        let encrypted = psk_ctx
            .encrypt(&header_bytes, &join.to_bytes(), 0, 0, 0)
            .expect("JOIN encrypt");
        let wire = build_wire_packet(&header_bytes, &encrypted);
        joiner_sock.send_to(&wire, host_addr).unwrap();
    }

    // ========================================================================
    // Step 2: Host receives JOIN, adds peer, sends PEER_LIST
    // ========================================================================
    {
        let mut buf = [0u8; MAX_PACKET_LEN];
        let (len, src_addr) = recv_with_retry(&host_sock, &mut buf);
        assert_eq!(src_addr, joiner_addr, "JOIN src should be joiner");

        let (header_bytes, ciphertext) = parse_wire_packet(&buf[..len]).unwrap();
        let header = PacketHeader::from_bytes(&header_bytes).unwrap();
        assert_eq!(header.pkt_type, PKT_JOIN);

        // Host decrypts JOIN
        let psk_ctx = CryptoContext::new(&key, [0; 8]);
        let plaintext = psk_ctx
            .decrypt(
                &header_bytes,
                ciphertext,
                header.peer_id,
                header.seq_num,
                header.counter,
            )
            .expect("host decrypt JOIN");
        let _join = JoinPayload::from_bytes(&plaintext).unwrap();

        // Host registers the peer
        let new_id = host_pm.allocate_peer_id().unwrap();
        assert_eq!(new_id, 1, "joiner gets ID 1");
        host_pm.add_peer(new_id, src_addr).unwrap();
        if let Some(peer) = host_pm.get_peer_mut(new_id) {
            peer.state = vc_core::peer::PeerState::Connected;
            peer.touch();
        }

        // Host sends PEER_LIST
        let pl = host_pm.build_peer_list(host_addr);
        let pl_bytes = pl.to_bytes();
        let hdr = PacketHeader::new(PKT_PEER_LIST, host_pm.local_id, 0, 0);
        let hdr_bytes = hdr.to_bytes();
        let psk_ctx = CryptoContext::new(&key, [0; 8]);
        let encrypted = psk_ctx
            .encrypt(&hdr_bytes, &pl_bytes, host_pm.local_id, 0, 0)
            .expect("PEER_LIST encrypt");
        let wire = build_wire_packet(&hdr_bytes, &encrypted);
        host_sock.send_to(&wire, src_addr).unwrap();
    }

    // ========================================================================
    // Step 3: Joiner receives PEER_LIST, sets identity, sends HELLO
    // ========================================================================
    let joiner_hello_target: SocketAddr;
    {
        let mut buf = [0u8; MAX_PACKET_LEN];
        let (len, src_addr) = recv_with_retry(&joiner_sock, &mut buf);
        assert_eq!(src_addr, host_addr);

        let (header_bytes, ciphertext) = parse_wire_packet(&buf[..len]).unwrap();
        let header = PacketHeader::from_bytes(&header_bytes).unwrap();
        assert_eq!(header.pkt_type, PKT_PEER_LIST);

        let psk_ctx = CryptoContext::new(&key, [0; 8]);
        let plaintext = psk_ctx
            .decrypt(
                &header_bytes,
                ciphertext,
                header.peer_id,
                header.seq_num,
                header.counter,
            )
            .expect("joiner decrypt PEER_LIST");
        let pl = PeerListPayload::from_bytes(&plaintext).unwrap();
        assert_eq!(pl.session_id, host_session_id);

        let my_id = pl.peers.iter().map(|(id, _)| *id).max().unwrap();
        joiner_pm.set_identity(my_id, pl.session_id);
        assert_eq!(joiner_pm.local_id, 1);
        assert_eq!(joiner_pm.session_id, host_session_id);

        // Send HELLO to host
        let peer_addr = if header.peer_id == 0 {
            src_addr // use actual source, not listed LAN IP
        } else {
            pl.peers
                .iter()
                .find(|(id, _)| *id == header.peer_id)
                .unwrap()
                .1
        };
        joiner_pm.add_peer(0, peer_addr).unwrap();

        let hdr = PacketHeader::new(PKT_HELLO, my_id, 0, 0);
        let hdr_bytes = hdr.to_bytes();
        let psk_ctx = CryptoContext::new(&key, pl.session_id);
        let encrypted = psk_ctx
            .encrypt(&hdr_bytes, &[], my_id, 0, 0)
            .expect("HELLO encrypt");
        let wire = build_wire_packet(&hdr_bytes, &encrypted);
        joiner_sock.send_to(&wire, peer_addr).unwrap();

        if let Some(peer) = joiner_pm.get_peer_mut(0) {
            peer.state = vc_core::peer::PeerState::HelloSent;
        }
        joiner_hello_target = peer_addr;
    }
    assert_eq!(joiner_hello_target, host_addr);

    // ========================================================================
    // Step 4: Host receives HELLO, sends HELLO_ACK
    // ========================================================================
    {
        let mut buf = [0u8; MAX_PACKET_LEN];
        let (len, src_addr) = recv_with_retry(&host_sock, &mut buf);
        assert_eq!(src_addr, joiner_addr);

        let (header_bytes, ciphertext) = parse_wire_packet(&buf[..len]).unwrap();
        let header = PacketHeader::from_bytes(&header_bytes).unwrap();
        assert_eq!(header.pkt_type, PKT_HELLO);

        let psk_ctx = CryptoContext::new(&key, host_pm.session_id);
        psk_ctx
            .decrypt(
                &header_bytes,
                ciphertext,
                header.peer_id,
                header.seq_num,
                header.counter,
            )
            .expect("host decrypt HELLO");

        if let Some(peer) = host_pm.get_peer_mut(header.peer_id) {
            peer.state = vc_core::peer::PeerState::Connected;
            peer.touch();
        }

        // Send HELLO_ACK
        let hdr = PacketHeader::new(PKT_HELLO_ACK, host_pm.local_id, 0, 0);
        let hdr_bytes = hdr.to_bytes();
        let psk_ctx = CryptoContext::new(&key, host_pm.session_id);
        let encrypted = psk_ctx
            .encrypt(&hdr_bytes, &[], host_pm.local_id, 0, 0)
            .expect("HELLO_ACK encrypt");
        let wire = build_wire_packet(&hdr_bytes, &encrypted);
        host_sock.send_to(&wire, src_addr).unwrap();
    }

    // ========================================================================
    // Step 5: Joiner receives HELLO_ACK → peer is Connected
    // ========================================================================
    {
        let mut buf = [0u8; MAX_PACKET_LEN];
        let (len, src_addr) = recv_with_retry(&joiner_sock, &mut buf);
        assert_eq!(src_addr, host_addr);

        let (header_bytes, ciphertext) = parse_wire_packet(&buf[..len]).unwrap();
        let header = PacketHeader::from_bytes(&header_bytes).unwrap();
        assert_eq!(header.pkt_type, PKT_HELLO_ACK);

        let psk_ctx = CryptoContext::new(&key, joiner_pm.session_id);
        psk_ctx
            .decrypt(
                &header_bytes,
                ciphertext,
                header.peer_id,
                header.seq_num,
                header.counter,
            )
            .expect("joiner decrypt HELLO_ACK");

        if let Some(peer) = joiner_pm.get_peer_mut(header.peer_id) {
            peer.state = vc_core::peer::PeerState::Connected;
        }
    }

    // Both sides should be Connected
    assert!(host_pm.get_peer(1).unwrap().is_connected(), "host→joiner");
    assert!(joiner_pm.get_peer(0).unwrap().is_connected(), "joiner→host");

    // Header length sanity
    assert_eq!(HEADER_LEN, 8);

    // ========================================================================
    // Step 6: Post-handshake audio flow — regression for PR #12 fix
    //
    // The v0.1.7 bug: sender's crypto_ctx internal send_counter diverged
    // from what the receiver used for nonce reconstruction, so every audio
    // packet after the first keepalive silently failed to decrypt. PR #12
    // carries the counter in the header so both sides stay in sync.
    //
    // This step simulates what Session::run() does: keepalives burn counter
    // slots, then audio packets are sent with monotonic counters, and the
    // receiver must decrypt them using `header.counter`.
    // ========================================================================
    let host_session_id_actual = host_pm.session_id;
    let mut host_crypto = CryptoContext::new(&key, host_session_id_actual);
    let joiner_crypto = CryptoContext::new(&key, joiner_pm.session_id);

    // Burn a few counter slots on the host side (simulating keepalive/ping).
    // If the audio path still worked after this in v0.1.7 it would be
    // accidental. In v0.1.8 each send stamps header.counter with the reserved
    // value so the receiver can reconstruct the nonce.
    for _ in 0..3 {
        let _ = host_crypto.next_counter().unwrap();
    }

    // Host sends a synthetic audio packet (plaintext stand-in for Opus data)
    let fake_opus = b"fake opus frame";
    let counter = host_crypto.next_counter().unwrap();
    assert_eq!(counter, 3, "expected 4th reservation");
    let audio_header = PacketHeader::new(PKT_AUDIO, host_pm.local_id, 42, counter);
    let audio_header_bytes = audio_header.to_bytes();
    let encrypted = host_crypto
        .encrypt(
            &audio_header_bytes,
            fake_opus,
            host_pm.local_id,
            42,
            counter,
        )
        .expect("audio encrypt");
    let wire = build_wire_packet(&audio_header_bytes, &encrypted);
    host_sock.send_to(&wire, joiner_addr).unwrap();

    // Joiner receives and decrypts using header.counter
    let mut buf = [0u8; MAX_PACKET_LEN];
    let (len, src_addr) = recv_with_retry(&joiner_sock, &mut buf);
    assert_eq!(src_addr, host_addr);
    let (header_bytes, ciphertext) = parse_wire_packet(&buf[..len]).unwrap();
    let header = PacketHeader::from_bytes(&header_bytes).unwrap();
    assert_eq!(header.pkt_type, PKT_AUDIO);
    assert_eq!(header.counter, 3, "counter must survive the wire");
    let plaintext = joiner_crypto
        .decrypt(
            &header_bytes,
            ciphertext,
            header.peer_id,
            header.seq_num,
            header.counter,
        )
        .expect("joiner decrypt audio — this was the v0.1.7 regression");
    assert_eq!(plaintext, fake_opus);
}
