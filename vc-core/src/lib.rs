pub mod audio;
pub mod codec;
pub mod crypto;
pub mod jitter;
pub mod mixer;
pub mod net;
pub mod peer;

use anyhow::{anyhow, Result};
use ringbuf::traits::{Consumer, Producer};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use codec::{FRAME_DURATION_US, FRAME_SAMPLES};
use crypto::CryptoContext;
use net::*;
use peer::*;

/// Configuration for a voice chat session.
pub struct SessionConfig {
    pub bind_addr: SocketAddr,
    pub passphrase: String,
    pub is_host: bool,
    /// If joining, the host address to connect to.
    pub host_addr: Option<SocketAddr>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
}

/// Main voice chat session orchestrating all threads.
pub struct Session {
    config: SessionConfig,
    running: Arc<AtomicBool>,
}

impl Session {
    pub fn new(config: SessionConfig) -> Self {
        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Run the voice chat session. Blocks until stopped.
    pub fn run(&self) -> Result<()> {
        self.running.store(true, Ordering::SeqCst);

        let key = crypto::derive_key_from_passphrase(&self.config.passphrase)?;
        let session_id: [u8; 8] = if self.config.is_host {
            let mut id = [0u8; 8];
            use rand::RngCore;
            rand::thread_rng().fill_bytes(&mut id);
            id
        } else {
            [0; 8] // Will be set from PEER_LIST
        };

        let mut peer_mgr = if self.config.is_host {
            PeerManager::new_host(session_id)
        } else {
            PeerManager::new_joiner()
        };

        let mut crypto_ctx = CryptoContext::new(&key, session_id);
        let socket = net::UdpSocket::bind(self.config.bind_addr)?;
        log::info!("Bound to {}", socket.local_addr()?);

        // Audio capture ring buffer: capture thread → encode thread
        let (capture_prod, mut capture_cons) = audio::create_audio_ring_buffer();
        // Playout ring buffer: decode/mix thread → playout thread
        let (mut playout_prod, playout_cons) = audio::create_audio_ring_buffer();

        // Start audio I/O streams
        let _capture_stream = audio::start_capture(
            capture_prod,
            self.running.clone(),
            self.config.input_device.as_deref(),
        )?;
        let _playout_stream = audio::start_playout(
            playout_cons,
            self.running.clone(),
            self.config.output_device.as_deref(),
        )?;

        let mut encoder = codec::Encoder::new()?;

        // If joining, send JOIN to host
        if !self.config.is_host {
            if let Some(host_addr) = self.config.host_addr {
                let join_payload = JoinPayload {
                    listen_port: self.config.bind_addr.port(),
                };
                let header = PacketHeader::new(PKT_JOIN, 0, 0, 0);
                let header_bytes = header.to_bytes();
                let payload_bytes = join_payload.to_bytes();

                // Encrypt JOIN with PSK
                let (encrypted, _counter) =
                    crypto_ctx.encrypt(&header_bytes, &payload_bytes, 0, 0)?;
                let wire = build_wire_packet(&header_bytes, &encrypted);
                socket.send_to(&wire, host_addr)?;
                log::info!("Sent JOIN to {host_addr}");
            }
        }

        // Accumulator for capture samples (building 2.5ms frames)
        let mut capture_accum = Vec::with_capacity(FRAME_SAMPLES);
        let mut recv_buf = [0u8; MAX_PACKET_LEN];

        let mut last_keepalive = Instant::now();
        let mut last_adapt = Instant::now();

        socket.set_read_timeout(Some(Duration::from_millis(1)))?;

        while self.running.load(Ordering::Relaxed) {
            // --- Receive incoming packets ---
            match socket.recv_from(&mut recv_buf) {
                Ok((len, src_addr)) => {
                    if let Err(e) = self.handle_incoming(
                        &recv_buf[..len],
                        src_addr,
                        &mut peer_mgr,
                        &mut crypto_ctx,
                        &key,
                        &socket,
                    ) {
                        log::debug!("Packet from {src_addr} rejected: {e}");
                    }
                }
                Err(e) => {
                    // Timeout is expected (non-blocking with 1ms timeout)
                    if e.kind() != std::io::ErrorKind::WouldBlock
                        && e.kind() != std::io::ErrorKind::TimedOut
                    {
                        log::error!("recv error: {e}");
                    }
                }
            }

            // --- Capture → Encode → Encrypt → Send ---
            // Drain samples from capture ring buffer
            let mut temp_buf = [0.0f32; 256];
            let read = capture_cons.pop_slice(&mut temp_buf);
            if read > 0 {
                capture_accum.extend_from_slice(&temp_buf[..read]);
            }

            // Process complete frames
            while capture_accum.len() >= FRAME_SAMPLES {
                let frame_f32: Vec<f32> = capture_accum.drain(..FRAME_SAMPLES).collect();
                let frame_i16 = codec::f32_to_i16(&frame_f32);

                match encoder.encode(&frame_i16) {
                    Ok(opus_data) => {
                        let connected = peer_mgr.connected_peers();
                        for (peer_id, peer_addr) in &connected {
                            if let Some(peer) = peer_mgr.get_peer_mut(*peer_id) {
                                let seq = peer.next_seq();
                                let ts = peer.advance_timestamp();
                                let header =
                                    PacketHeader::new(PKT_AUDIO, peer_mgr.local_id, seq, ts);
                                let header_bytes = header.to_bytes();

                                match crypto_ctx.encrypt(
                                    &header_bytes,
                                    &opus_data,
                                    peer_mgr.local_id,
                                    seq,
                                ) {
                                    Ok((encrypted, _counter)) => {
                                        let wire =
                                            build_wire_packet(&header_bytes, &encrypted);
                                        if let Err(e) = socket.send_to(&wire, *peer_addr) {
                                            log::warn!("send to {peer_addr}: {e}");
                                        }
                                    }
                                    Err(e) => log::error!("encrypt: {e}"),
                                }
                            }
                        }
                    }
                    Err(e) => log::error!("encode: {e}"),
                }
            }

            // --- Decode + Mix → Playout ---
            let mut mixed = vec![0.0f32; FRAME_SAMPLES];
            let mut peer_bufs: Vec<Vec<f32>> = Vec::new();

            for peer in peer_mgr.peers.values_mut() {
                if peer.state != PeerState::Connected {
                    continue;
                }
                match peer.jitter_buffer.pop() {
                    Some(pcm_i16) => {
                        peer_bufs.push(codec::i16_to_f32(&pcm_i16));
                    }
                    None => {
                        // PLC
                        match peer.decoder.decode_plc() {
                            Ok(plc) => peer_bufs.push(codec::i16_to_f32(&plc)),
                            Err(e) => log::debug!("PLC error for peer {}: {e}", peer.id),
                        }
                    }
                }
            }

            if !peer_bufs.is_empty() {
                let refs: Vec<&[f32]> = peer_bufs.iter().map(|b| b.as_slice()).collect();
                mixer::mix_peers(&refs, &mut mixed);
                let written = playout_prod.push_slice(&mixed);
                if written < mixed.len() {
                    log::debug!("playout ring buffer full, dropped samples");
                }
            }

            // --- Keepalive ---
            if last_keepalive.elapsed().as_secs_f64() >= 1.0 {
                let connected = peer_mgr.connected_peers();
                for (_peer_id, peer_addr) in &connected {
                    let header = PacketHeader::new(PKT_KEEPALIVE, peer_mgr.local_id, 0, 0);
                    let header_bytes = header.to_bytes();
                    match crypto_ctx.encrypt(&header_bytes, &[], peer_mgr.local_id, 0) {
                        Ok((encrypted, _)) => {
                            let wire = build_wire_packet(&header_bytes, &encrypted);
                            let _ = socket.send_to(&wire, *peer_addr);
                        }
                        Err(e) => log::debug!("keepalive encrypt: {e}"),
                    }
                }
                peer_mgr.check_timeouts();
                last_keepalive = Instant::now();
            }

            // --- Jitter buffer adaptation ---
            if last_adapt.elapsed().as_secs_f64() >= 0.5 {
                let elapsed = last_adapt.elapsed().as_secs_f64();
                for peer in peer_mgr.peers.values_mut() {
                    peer.jitter_buffer.adapt(elapsed);
                }
                last_adapt = Instant::now();
            }
        }

        log::info!("Session stopped");
        Ok(())
    }

    fn handle_incoming(
        &self,
        data: &[u8],
        src_addr: SocketAddr,
        peer_mgr: &mut PeerManager,
        crypto_ctx: &mut CryptoContext,
        key: &[u8; 32],
        socket: &net::UdpSocket,
    ) -> Result<()> {
        let (header_bytes, ciphertext) = parse_wire_packet(data)?;
        let header = PacketHeader::from_bytes(&header_bytes)?;

        match header.pkt_type {
            PKT_AUDIO => {
                let peer_id = header.peer_id;

                // We need the counter to construct the nonce for decryption.
                // The counter is derived from the send_counter on the sender side.
                // Since we can't transmit it, we use the sequence number as a proxy
                // and maintain a mapping. For simplicity, we use seq_num as counter.
                let counter = header.seq_num as u32;

                let plaintext =
                    crypto_ctx.decrypt(&header_bytes, ciphertext, peer_id, header.seq_num, counter)?;

                if let Some(peer) = peer_mgr.get_peer_mut(peer_id) {
                    peer.touch();
                    if !peer.replay_filter.check_and_accept(counter) {
                        return Err(anyhow!("replay detected for peer {peer_id}"));
                    }
                    let pcm = peer.decoder.decode(&plaintext)?;
                    peer.jitter_buffer.push(header.seq_num, pcm);
                }
            }

            PKT_JOIN => {
                if !peer_mgr.is_host {
                    return Err(anyhow!("non-host received JOIN"));
                }
                // Decrypt with PSK
                let psk_ctx = CryptoContext::new(key, peer_mgr.session_id);
                let plaintext =
                    psk_ctx.decrypt(&header_bytes, ciphertext, 0, 0, 0)?;
                let _join = JoinPayload::from_bytes(&plaintext)?;

                let new_id = peer_mgr.allocate_peer_id()?;
                peer_mgr.add_peer(new_id, src_addr)?;
                if let Some(peer) = peer_mgr.get_peer_mut(new_id) {
                    peer.state = PeerState::Connected;
                    peer.touch();
                }

                // Send PEER_LIST back
                let local_addr = socket.local_addr()?;
                let pl = peer_mgr.build_peer_list(local_addr);
                let pl_bytes = pl.to_bytes();
                let hdr = PacketHeader::new(PKT_PEER_LIST, peer_mgr.local_id, 0, 0);
                let hdr_bytes = hdr.to_bytes();
                let mut psk_ctx = CryptoContext::new(key, peer_mgr.session_id);
                let (encrypted, _) =
                    psk_ctx.encrypt(&hdr_bytes, &pl_bytes, peer_mgr.local_id, 0)?;
                let wire = build_wire_packet(&hdr_bytes, &encrypted);
                socket.send_to(&wire, src_addr)?;
                log::info!("Peer {new_id} joined from {src_addr}");
            }

            PKT_PEER_LIST => {
                // Decrypt with PSK
                let psk_ctx = CryptoContext::new(key, [0; 8]); // session_id unknown yet
                // Try decryption — we don't know session_id yet, use zeros
                // Actually, the host encrypted with their session_id.
                // For the joiner, we need to try with a temporary context.
                // The proper approach: use header peer_id + seq 0 + counter 0
                let plaintext =
                    psk_ctx.decrypt(&header_bytes, ciphertext, header.peer_id, 0, 0)?;
                let pl = PeerListPayload::from_bytes(&plaintext)?;

                // Find our assigned ID (the one not matching any peer in the list)
                // Actually, the host includes us in the list. Our ID is the highest one.
                let my_id = pl
                    .peers
                    .iter()
                    .map(|(id, _)| *id)
                    .max()
                    .ok_or_else(|| anyhow!("empty peer list"))?;

                peer_mgr.set_identity(my_id, pl.session_id);
                *crypto_ctx = CryptoContext::new(key, pl.session_id);

                log::info!(
                    "Received peer list: assigned ID {my_id}, session {:02x?}",
                    pl.session_id
                );

                // Add all other peers and send HELLO to each
                for (id, addr) in &pl.peers {
                    if *id == my_id {
                        continue;
                    }
                    peer_mgr.add_peer(*id, *addr)?;

                    // Send HELLO
                    let hdr = PacketHeader::new(PKT_HELLO, my_id, 0, 0);
                    let hdr_bytes = hdr.to_bytes();
                    let mut psk_ctx = CryptoContext::new(key, pl.session_id);
                    let (encrypted, _) = psk_ctx.encrypt(&hdr_bytes, &[], my_id, 0)?;
                    let wire = build_wire_packet(&hdr_bytes, &encrypted);
                    socket.send_to(&wire, *addr)?;
                    log::info!("Sent HELLO to peer {id} at {addr}");

                    if let Some(peer) = peer_mgr.get_peer_mut(*id) {
                        peer.state = PeerState::HelloSent;
                    }
                }
            }

            PKT_HELLO => {
                let psk_ctx = CryptoContext::new(key, peer_mgr.session_id);
                psk_ctx.decrypt(&header_bytes, ciphertext, header.peer_id, 0, 0)?;

                let peer_id = header.peer_id;
                if peer_mgr.get_peer(peer_id).is_none() {
                    peer_mgr.add_peer(peer_id, src_addr)?;
                }
                if let Some(peer) = peer_mgr.get_peer_mut(peer_id) {
                    peer.state = PeerState::Connected;
                    peer.touch();
                }

                // Send HELLO_ACK
                let hdr = PacketHeader::new(PKT_HELLO_ACK, peer_mgr.local_id, 0, 0);
                let hdr_bytes = hdr.to_bytes();
                let mut psk_ctx = CryptoContext::new(key, peer_mgr.session_id);
                let (encrypted, _) =
                    psk_ctx.encrypt(&hdr_bytes, &[], peer_mgr.local_id, 0)?;
                let wire = build_wire_packet(&hdr_bytes, &encrypted);
                socket.send_to(&wire, src_addr)?;
                log::info!("Received HELLO from peer {peer_id}, sent HELLO_ACK");
            }

            PKT_HELLO_ACK => {
                let psk_ctx = CryptoContext::new(key, peer_mgr.session_id);
                psk_ctx.decrypt(&header_bytes, ciphertext, header.peer_id, 0, 0)?;

                let peer_id = header.peer_id;
                if let Some(peer) = peer_mgr.get_peer_mut(peer_id) {
                    peer.state = PeerState::Connected;
                    peer.touch();
                    log::info!("Peer {peer_id} connected (HELLO_ACK received)");
                }
            }

            PKT_KEEPALIVE => {
                if let Some(peer_id) = peer_mgr.peer_by_addr(&src_addr) {
                    if let Some(peer) = peer_mgr.get_peer_mut(peer_id) {
                        peer.touch();
                    }
                }
            }

            _ => {
                return Err(anyhow!("unknown packet type: {}", header.pkt_type));
            }
        }

        Ok(())
    }

    /// Stop the session.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

/// Run a loopback test: capture → encode → decode → playout (no network).
pub fn run_loopback(
    input_device: Option<&str>,
    output_device: Option<&str>,
    running: Arc<AtomicBool>,
) -> Result<()> {
    running.store(true, Ordering::SeqCst);

    let (capture_prod, mut capture_cons) = audio::create_audio_ring_buffer();
    let (mut playout_prod, playout_cons) = audio::create_audio_ring_buffer();

    let _capture = audio::start_capture(capture_prod, running.clone(), input_device)?;
    let _playout = audio::start_playout(playout_cons, running.clone(), output_device)?;

    let mut encoder = codec::Encoder::new()?;
    let mut decoder = codec::Decoder::new()?;

    let mut accum = Vec::with_capacity(FRAME_SAMPLES);
    let mut temp = [0.0f32; 256];

    log::info!("Loopback test running (Ctrl+C to stop)");

    while running.load(Ordering::Relaxed) {
        let read = capture_cons.pop_slice(&mut temp);
        if read > 0 {
            accum.extend_from_slice(&temp[..read]);
        }

        while accum.len() >= FRAME_SAMPLES {
            let frame_f32: Vec<f32> = accum.drain(..FRAME_SAMPLES).collect();
            let frame_i16 = codec::f32_to_i16(&frame_f32);

            match encoder.encode(&frame_i16) {
                Ok(opus_data) => match decoder.decode(&opus_data) {
                    Ok(decoded) => {
                        let out_f32 = codec::i16_to_f32(&decoded);
                        playout_prod.push_slice(&out_f32);
                    }
                    Err(e) => log::error!("decode: {e}"),
                },
                Err(e) => log::error!("encode: {e}"),
            }
        }

        // Sleep briefly to avoid busy-spinning
        std::thread::sleep(Duration::from_micros(FRAME_DURATION_US / 2));
    }

    Ok(())
}
