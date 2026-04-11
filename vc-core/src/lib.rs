pub mod audio;
pub mod codec;
pub mod crypto;
pub mod dsp;
pub mod input;
pub mod jitter;
pub mod key_exchange;
pub mod latency;
pub mod mixer;
pub mod net;
pub mod peer;
pub mod upnp;
pub mod vad;

use anyhow::{anyhow, Result};
use ringbuf::traits::{Consumer, Producer};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Determine our routable local IP for reaching a given remote address.
/// Uses the OS routing table via a temporary connected UDP socket (no data sent).
fn resolve_local_ip(remote: SocketAddr) -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(remote).ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

use codec::{FRAME_DURATION_US, FRAME_SAMPLES};
use crypto::CryptoContext;
use dsp::NoiseSuppressor;
use input::{InputGate, InputMode};
use key_exchange::{KeyExchange, KeyExchangePayload};
use latency::PingPayload;
use net::*;
use peer::*;
use vad::{VadConfig, VoiceActivityDetector};

/// Configuration for a voice chat session.
pub struct SessionConfig {
    pub bind_addr: SocketAddr,
    pub passphrase: String,
    pub is_host: bool,
    pub host_addr: Option<SocketAddr>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// Enable RNNoise noise suppression (adds ~7.5ms latency).
    pub noise_suppression: bool,
    /// Input transmission mode.
    pub input_mode: InputMode,
    /// Voice activity detection configuration.
    pub vad_config: VadConfig,
    /// Attempt UPnP/IGD port forwarding when hosting.
    pub upnp: bool,
}

/// Peer information for display in UI.
#[derive(Clone, Debug)]
pub struct PeerDisplayInfo {
    pub id: u8,
    pub addr: SocketAddr,
    pub state: PeerState,
    pub latency: latency::LatencyStats,
    pub key_exchange_done: bool,
}

/// Thread-safe shared state between Session and UI.
pub struct SessionShared {
    // Controls (UI -> Session)
    pub noise_suppression: AtomicBool,
    pub ptt_active: AtomicBool,
    pub input_mode: Mutex<InputMode>,

    // Stats (Session -> UI)
    pub is_transmitting: AtomicBool,
    pub peer_info: Mutex<Vec<PeerDisplayInfo>>,
    /// External address from UPnP mapping (if successful).
    pub external_addr: Mutex<Option<SocketAddr>>,
}

impl SessionShared {
    fn new(ns_enabled: bool, mode: InputMode) -> Self {
        Self {
            noise_suppression: AtomicBool::new(ns_enabled),
            ptt_active: AtomicBool::new(false),
            input_mode: Mutex::new(mode),
            is_transmitting: AtomicBool::new(false),
            peer_info: Mutex::new(Vec::new()),
            external_addr: Mutex::new(None),
        }
    }
}

/// Main voice chat session orchestrating all components.
pub struct Session {
    config: SessionConfig,
    running: Arc<AtomicBool>,
    shared: Arc<SessionShared>,
}

/// Interval between latency pings (seconds).
const PING_INTERVAL_SECS: f64 = 2.0;

impl Session {
    pub fn new(config: SessionConfig) -> Self {
        let shared = Arc::new(SessionShared::new(
            config.noise_suppression,
            config.input_mode.clone(),
        ));
        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            shared,
        }
    }

    /// Get a handle to the shared state for UI communication.
    pub fn shared(&self) -> Arc<SessionShared> {
        self.shared.clone()
    }

    /// Get a handle to the running flag.
    pub fn running_flag(&self) -> Arc<AtomicBool> {
        self.running.clone()
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
            [0; 8]
        };

        let mut peer_mgr = if self.config.is_host {
            PeerManager::new_host(session_id)
        } else {
            PeerManager::new_joiner()
        };

        let mut crypto_ctx = CryptoContext::new(&key, session_id);
        let socket = net::UdpSocket::bind(self.config.bind_addr)?;
        log::info!("Bound to {}", socket.local_addr()?);

        // UPnP port forwarding (host only)
        let _upnp_mapping = if self.config.is_host && self.config.upnp {
            let local_addr = socket.local_addr()?;
            // Resolve actual LAN IP if bound to 0.0.0.0
            let map_addr = if local_addr.ip().is_unspecified() {
                let ip = resolve_local_ip("8.8.8.8:53".parse().unwrap()).unwrap_or(local_addr.ip());
                SocketAddr::new(ip, local_addr.port())
            } else {
                local_addr
            };
            match upnp::UpnpMapping::setup(map_addr) {
                Ok((ext_addr, mapping)) => {
                    log::info!("UPnP: peers can connect to {ext_addr}");
                    if let Ok(mut addr) = self.shared.external_addr.lock() {
                        *addr = Some(ext_addr);
                    }
                    Some(mapping)
                }
                Err(e) => {
                    log::warn!("UPnP failed: {e} — manual port forwarding required");
                    None
                }
            }
        } else {
            None
        };

        // Audio ring buffers
        let (capture_prod, mut capture_cons) = audio::create_audio_ring_buffer();
        let (mut playout_prod, playout_cons) = audio::create_audio_ring_buffer();

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

        // Phase 4: Noise suppression
        let mut noise_suppressor = NoiseSuppressor::new();
        noise_suppressor.set_enabled(self.config.noise_suppression);

        // Phase 4: VAD + Input gate
        let mut vad = VoiceActivityDetector::new(self.config.vad_config.clone());
        let mut input_gate = InputGate::new(self.config.input_mode.clone());
        // Link PTT flag to shared state
        let ptt_flag = input_gate.ptt_flag();
        {
            // Sync PTT from shared state on each iteration
        }

        // Phase 4: X25519 key exchange
        let key_exchange = KeyExchange::new();

        // If joining, send JOIN to host.
        //
        // Uses a fresh zero-session crypto context (counter=0, session_id=0)
        // because the joiner doesn't yet know the session_id — that's exactly
        // what the host expects to decrypt this initial packet with.
        if !self.config.is_host {
            if let Some(host_addr) = self.config.host_addr {
                let join_payload = JoinPayload {
                    listen_port: self.config.bind_addr.port(),
                };
                let header = PacketHeader::new(PKT_JOIN, 0, 0, 0);
                let header_bytes = header.to_bytes();
                let payload_bytes = join_payload.to_bytes();

                let psk_ctx = CryptoContext::new(&key, [0; 8]);
                let encrypted = psk_ctx.encrypt(&header_bytes, &payload_bytes, 0, 0, 0)?;
                let wire = build_wire_packet(&header_bytes, &encrypted);
                socket.send_to(&wire, host_addr)?;
                log::info!("Sent JOIN to {host_addr}");
            }
        }

        let mut capture_accum = Vec::with_capacity(FRAME_SAMPLES);
        let mut recv_buf = [0u8; MAX_PACKET_LEN];

        let mut last_keepalive = Instant::now();
        let mut last_adapt = Instant::now();
        let mut last_ping = Instant::now();
        let mut last_ui_update = Instant::now();

        socket.set_read_timeout(Some(Duration::from_millis(1)))?;

        while self.running.load(Ordering::Relaxed) {
            // Sync controls from shared state
            noise_suppressor.set_enabled(self.shared.noise_suppression.load(Ordering::Relaxed));
            ptt_flag.store(
                self.shared.ptt_active.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            if let Ok(mode) = self.shared.input_mode.lock() {
                input_gate.set_mode(mode.clone());
            }

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
                        &key_exchange,
                    ) {
                        log::debug!("Packet from {src_addr} rejected: {e}");
                    }
                }
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::WouldBlock
                        && e.kind() != std::io::ErrorKind::TimedOut
                    {
                        log::error!("recv error: {e}");
                    }
                }
            }

            // --- Capture -> [Noise Suppression] -> [VAD/Gate] -> Encode -> Encrypt -> Send ---
            let mut temp_buf = [0.0f32; 4800];
            let read = capture_cons.pop_slice(&mut temp_buf);
            if read > 0 {
                capture_accum.extend_from_slice(&temp_buf[..read]);
            }

            while capture_accum.len() >= FRAME_SAMPLES {
                let frame_f32: Vec<f32> = capture_accum.drain(..FRAME_SAMPLES).collect();

                // Noise suppression (may buffer 4 frames before outputting)
                let processed = noise_suppressor.process(&frame_f32);

                if let Some(denoised) = processed {
                    let rnnoise_vad = if noise_suppressor.is_enabled() {
                        Some(noise_suppressor.vad_probability())
                    } else {
                        None
                    };

                    for chunk in denoised.chunks(FRAME_SAMPLES) {
                        if chunk.len() < FRAME_SAMPLES {
                            break;
                        }

                        // VAD check
                        let voice_active = vad.detect(chunk, rnnoise_vad);
                        input_gate.set_vad_active(voice_active);

                        if !input_gate.should_transmit() {
                            continue;
                        }

                        let frame_i16 = codec::f32_to_i16(chunk);
                        match encoder.encode(&frame_i16) {
                            Ok(opus_data) => {
                                let local_id = peer_mgr.local_id;
                                let connected = peer_mgr.connected_peers();
                                for (peer_id, peer_addr) in &connected {
                                    if let Some(peer) = peer_mgr.get_peer_mut(*peer_id) {
                                        let seq = peer.next_seq();

                                        // Use per-peer key if key exchange is complete.
                                        let use_peer_key = peer.kx_sent
                                            && peer.kx_received
                                            && peer.peer_crypto.is_some();

                                        // Reserve the monotonic counter *before*
                                        // building the header so the AAD and
                                        // the XChaCha20 nonce agree on the same
                                        // value. Receiver reads header.counter.
                                        let encrypt_result = if use_peer_key {
                                            let peer_ctx = peer.peer_crypto.as_mut().unwrap();
                                            match peer_ctx.next_counter() {
                                                Ok(counter) => {
                                                    let header = PacketHeader::new(
                                                        PKT_AUDIO, local_id, seq, counter,
                                                    );
                                                    let header_bytes = header.to_bytes();
                                                    peer_ctx
                                                        .encrypt(
                                                            &header_bytes,
                                                            &opus_data,
                                                            local_id,
                                                            seq,
                                                            counter,
                                                        )
                                                        .map(|ct| (header_bytes, ct))
                                                }
                                                Err(e) => Err(e),
                                            }
                                        } else {
                                            match crypto_ctx.next_counter() {
                                                Ok(counter) => {
                                                    let header = PacketHeader::new(
                                                        PKT_AUDIO, local_id, seq, counter,
                                                    );
                                                    let header_bytes = header.to_bytes();
                                                    crypto_ctx
                                                        .encrypt(
                                                            &header_bytes,
                                                            &opus_data,
                                                            local_id,
                                                            seq,
                                                            counter,
                                                        )
                                                        .map(|ct| (header_bytes, ct))
                                                }
                                                Err(e) => Err(e),
                                            }
                                        };

                                        match encrypt_result {
                                            Ok((header_bytes, encrypted)) => {
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
                }
            }

            // Update transmit status for UI
            self.shared
                .is_transmitting
                .store(input_gate.is_transmitting(), Ordering::Relaxed);

            // --- Decode + Mix -> Playout ---
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
                    None => match peer.decoder.decode_plc() {
                        Ok(plc) => peer_bufs.push(codec::i16_to_f32(&plc)),
                        Err(e) => log::debug!("PLC error for peer {}: {e}", peer.id),
                    },
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
                let local_id = peer_mgr.local_id;
                let connected = peer_mgr.connected_peers();
                for (_peer_id, peer_addr) in &connected {
                    let counter = match crypto_ctx.next_counter() {
                        Ok(c) => c,
                        Err(e) => {
                            log::debug!("keepalive counter: {e}");
                            continue;
                        }
                    };
                    let header = PacketHeader::new(PKT_KEEPALIVE, local_id, 0, counter);
                    let header_bytes = header.to_bytes();
                    match crypto_ctx.encrypt(&header_bytes, &[], local_id, 0, counter) {
                        Ok(encrypted) => {
                            let wire = build_wire_packet(&header_bytes, &encrypted);
                            let _ = socket.send_to(&wire, *peer_addr);
                        }
                        Err(e) => log::debug!("keepalive encrypt: {e}"),
                    }
                }
                peer_mgr.check_timeouts();
                last_keepalive = Instant::now();
            }

            // --- Latency pings ---
            if last_ping.elapsed().as_secs_f64() >= PING_INTERVAL_SECS {
                let local_id = peer_mgr.local_id;
                let connected = peer_mgr.connected_peers();
                for (peer_id, peer_addr) in &connected {
                    let ping_id = match peer_mgr.get_peer_mut(*peer_id) {
                        Some(peer) => peer.latency_tracker.ping_sent(),
                        None => continue,
                    };
                    let payload = PingPayload { ping_id }.to_bytes();
                    let counter = match crypto_ctx.next_counter() {
                        Ok(c) => c,
                        Err(e) => {
                            log::debug!("ping counter: {e}");
                            continue;
                        }
                    };
                    let header = PacketHeader::new(PKT_PING, local_id, 0, counter);
                    let header_bytes = header.to_bytes();
                    match crypto_ctx.encrypt(&header_bytes, &payload, local_id, 0, counter) {
                        Ok(encrypted) => {
                            let wire = build_wire_packet(&header_bytes, &encrypted);
                            let _ = socket.send_to(&wire, *peer_addr);
                        }
                        Err(e) => log::debug!("ping encrypt: {e}"),
                    }
                }

                // Send KEY_EXCHANGE to newly connected peers that haven't received it.
                // Fresh psk_ctx per send means counter is always 0.
                let connected = peer_mgr.connected_peers();
                for (peer_id, peer_addr) in &connected {
                    let needs_kx = peer_mgr
                        .get_peer(*peer_id)
                        .map(|p| !p.kx_sent)
                        .unwrap_or(false);
                    if needs_kx {
                        let kx_payload = KeyExchangePayload {
                            public_key: key_exchange.public_key_bytes(),
                        };
                        let kx_bytes = kx_payload.to_bytes();
                        let hdr = PacketHeader::new(PKT_KEY_EXCHANGE, local_id, 0, 0);
                        let hdr_bytes = hdr.to_bytes();
                        let psk_ctx = CryptoContext::new(&key, peer_mgr.session_id);
                        if let Ok(encrypted) =
                            psk_ctx.encrypt(&hdr_bytes, &kx_bytes, local_id, 0, 0)
                        {
                            let wire = build_wire_packet(&hdr_bytes, &encrypted);
                            let _ = socket.send_to(&wire, *peer_addr);
                            if let Some(peer) = peer_mgr.get_peer_mut(*peer_id) {
                                peer.kx_sent = true;
                                log::info!("Sent KEY_EXCHANGE to peer {peer_id}");
                            }
                        }
                    }
                }

                last_ping = Instant::now();
            }

            // --- Jitter buffer adaptation ---
            if last_adapt.elapsed().as_secs_f64() >= 0.5 {
                let elapsed = last_adapt.elapsed().as_secs_f64();
                for peer in peer_mgr.peers.values_mut() {
                    peer.jitter_buffer.adapt(elapsed);
                }
                last_adapt = Instant::now();
            }

            // --- Update shared UI state ---
            if last_ui_update.elapsed().as_secs_f64() >= 0.25 {
                if let Ok(mut info) = self.shared.peer_info.lock() {
                    *info = peer_mgr
                        .peers
                        .values()
                        .map(|p| PeerDisplayInfo {
                            id: p.id,
                            addr: p.addr,
                            state: p.state,
                            latency: p.latency_tracker.stats().clone(),
                            key_exchange_done: p.kx_sent && p.kx_received,
                        })
                        .collect();
                }
                last_ui_update = Instant::now();
            }
        }

        log::info!("Session stopped");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_incoming(
        &self,
        data: &[u8],
        src_addr: SocketAddr,
        peer_mgr: &mut PeerManager,
        crypto_ctx: &mut CryptoContext,
        key: &[u8; 32],
        socket: &net::UdpSocket,
        key_exchange: &KeyExchange,
    ) -> Result<()> {
        let (header_bytes, ciphertext) = parse_wire_packet(data)?;
        let header = PacketHeader::from_bytes(&header_bytes)?;

        match header.pkt_type {
            PKT_AUDIO => {
                let peer_id = header.peer_id;
                // Counter is carried in the header by the sender (AAD-authenticated).
                let counter = header.counter;

                // Try per-peer key first if key exchange is complete, fall
                // back to the PSK context. The PSK path is needed for audio
                // sent before the sender upgraded to PFS.
                let try_peer_first = peer_mgr
                    .get_peer(peer_id)
                    .map(|p| p.kx_received && p.peer_crypto.is_some())
                    .unwrap_or(false);

                let (plaintext, used_peer_ctx) = if try_peer_first {
                    let peer_ctx = peer_mgr
                        .get_peer(peer_id)
                        .and_then(|p| p.peer_crypto.as_ref())
                        .unwrap();
                    match peer_ctx.decrypt(
                        &header_bytes,
                        ciphertext,
                        peer_id,
                        header.seq_num,
                        counter,
                    ) {
                        Ok(pt) => (pt, true),
                        Err(_) => {
                            // Fall back to PSK (peer may not have upgraded yet).
                            let pt = crypto_ctx.decrypt(
                                &header_bytes,
                                ciphertext,
                                peer_id,
                                header.seq_num,
                                counter,
                            )?;
                            (pt, false)
                        }
                    }
                } else {
                    let pt = crypto_ctx.decrypt(
                        &header_bytes,
                        ciphertext,
                        peer_id,
                        header.seq_num,
                        counter,
                    )?;
                    (pt, false)
                };

                if let Some(peer) = peer_mgr.get_peer_mut(peer_id) {
                    peer.touch();
                    // Per-context replay filters: the PSK and PFS senders each
                    // use their own monotonic counter, so we track replay state
                    // per-context to avoid rejecting legitimate PFS packets
                    // whose counter restarts from 0 after upgrade.
                    let filter = if used_peer_ctx {
                        &mut peer.peer_replay_filter
                    } else {
                        &mut peer.replay_filter
                    };
                    if !filter.check_and_accept(counter) {
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
                let psk_ctx = CryptoContext::new(key, [0; 8]);
                let plaintext = psk_ctx.decrypt(
                    &header_bytes,
                    ciphertext,
                    header.peer_id,
                    header.seq_num,
                    header.counter,
                )?;
                let _join = JoinPayload::from_bytes(&plaintext)?;

                let new_id = peer_mgr.allocate_peer_id()?;
                peer_mgr.add_peer(new_id, src_addr)?;
                if let Some(peer) = peer_mgr.get_peer_mut(new_id) {
                    peer.state = PeerState::Connected;
                    peer.touch();
                }

                let local_addr = socket.local_addr()?;
                // If bound to 0.0.0.0, resolve our actual routable IP for this peer
                let host_addr = if local_addr.ip().is_unspecified() {
                    let ip = resolve_local_ip(src_addr)
                        .ok_or_else(|| anyhow!("cannot determine local IP for peer list"))?;
                    SocketAddr::new(ip, local_addr.port())
                } else {
                    local_addr
                };
                let local_id = peer_mgr.local_id;
                let pl = peer_mgr.build_peer_list(host_addr);
                let pl_bytes = pl.to_bytes();
                let hdr = PacketHeader::new(PKT_PEER_LIST, local_id, 0, 0);
                let hdr_bytes = hdr.to_bytes();
                let psk_ctx = CryptoContext::new(key, [0; 8]);
                let encrypted = psk_ctx.encrypt(&hdr_bytes, &pl_bytes, local_id, 0, 0)?;
                let wire = build_wire_packet(&hdr_bytes, &encrypted);
                socket.send_to(&wire, src_addr)?;
                log::info!("Peer {new_id} joined from {src_addr}");
            }

            PKT_PEER_LIST => {
                let psk_ctx = CryptoContext::new(key, [0; 8]);
                let plaintext = psk_ctx.decrypt(
                    &header_bytes,
                    ciphertext,
                    header.peer_id,
                    header.seq_num,
                    header.counter,
                )?;
                let pl = PeerListPayload::from_bytes(&plaintext)?;

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

                for (id, addr) in &pl.peers {
                    if *id == my_id {
                        continue;
                    }
                    // For the peer that sent us the PEER_LIST (the host),
                    // use the actual source address instead of the listed
                    // address which may be a LAN IP behind NAT.
                    let peer_addr = if *id == header.peer_id {
                        src_addr
                    } else {
                        *addr
                    };
                    peer_mgr.add_peer(*id, peer_addr)?;

                    let hdr = PacketHeader::new(PKT_HELLO, my_id, 0, 0);
                    let hdr_bytes = hdr.to_bytes();
                    let psk_ctx = CryptoContext::new(key, pl.session_id);
                    let encrypted = psk_ctx.encrypt(&hdr_bytes, &[], my_id, 0, 0)?;
                    let wire = build_wire_packet(&hdr_bytes, &encrypted);
                    socket.send_to(&wire, peer_addr)?;
                    log::info!("Sent HELLO to peer {id} at {peer_addr}");

                    if let Some(peer) = peer_mgr.get_peer_mut(*id) {
                        peer.state = PeerState::HelloSent;
                    }
                }
            }

            PKT_HELLO => {
                let psk_ctx = CryptoContext::new(key, peer_mgr.session_id);
                psk_ctx.decrypt(
                    &header_bytes,
                    ciphertext,
                    header.peer_id,
                    header.seq_num,
                    header.counter,
                )?;

                let peer_id = header.peer_id;
                if peer_mgr.get_peer(peer_id).is_none() {
                    peer_mgr.add_peer(peer_id, src_addr)?;
                }
                if let Some(peer) = peer_mgr.get_peer_mut(peer_id) {
                    peer.state = PeerState::Connected;
                    peer.touch();
                }

                let local_id = peer_mgr.local_id;
                let hdr = PacketHeader::new(PKT_HELLO_ACK, local_id, 0, 0);
                let hdr_bytes = hdr.to_bytes();
                let psk_ctx = CryptoContext::new(key, peer_mgr.session_id);
                let encrypted = psk_ctx.encrypt(&hdr_bytes, &[], local_id, 0, 0)?;
                let wire = build_wire_packet(&hdr_bytes, &encrypted);
                socket.send_to(&wire, src_addr)?;
                log::info!("Received HELLO from peer {peer_id}, sent HELLO_ACK");
            }

            PKT_HELLO_ACK => {
                let psk_ctx = CryptoContext::new(key, peer_mgr.session_id);
                psk_ctx.decrypt(
                    &header_bytes,
                    ciphertext,
                    header.peer_id,
                    header.seq_num,
                    header.counter,
                )?;

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

            PKT_PING => {
                // Decrypt, parse ping_id, send PONG back
                let plaintext = crypto_ctx.decrypt(
                    &header_bytes,
                    ciphertext,
                    header.peer_id,
                    header.seq_num,
                    header.counter,
                )?;
                let ping = PingPayload::from_bytes(&plaintext)?;

                let local_id = peer_mgr.local_id;
                let pong_payload = ping.to_bytes();
                let pong_counter = match crypto_ctx.next_counter() {
                    Ok(c) => c,
                    Err(e) => {
                        log::debug!("pong counter: {e}");
                        return Ok(());
                    }
                };
                let hdr = PacketHeader::new(PKT_PONG, local_id, 0, pong_counter);
                let hdr_bytes = hdr.to_bytes();
                match crypto_ctx.encrypt(&hdr_bytes, &pong_payload, local_id, 0, pong_counter) {
                    Ok(encrypted) => {
                        let wire = build_wire_packet(&hdr_bytes, &encrypted);
                        let _ = socket.send_to(&wire, src_addr);
                    }
                    Err(e) => log::debug!("pong encrypt: {e}"),
                }

                if let Some(peer_id) = peer_mgr.peer_by_addr(&src_addr) {
                    if let Some(peer) = peer_mgr.get_peer_mut(peer_id) {
                        peer.touch();
                    }
                }
            }

            PKT_PONG => {
                let plaintext = crypto_ctx.decrypt(
                    &header_bytes,
                    ciphertext,
                    header.peer_id,
                    header.seq_num,
                    header.counter,
                )?;
                let pong = PingPayload::from_bytes(&plaintext)?;

                let peer_id = header.peer_id;
                if let Some(peer) = peer_mgr.get_peer_mut(peer_id) {
                    peer.touch();
                    if let Some(rtt) = peer.latency_tracker.pong_received(pong.ping_id) {
                        log::debug!(
                            "Peer {peer_id} RTT: {:.1}ms (avg: {:.1}ms)",
                            rtt as f64 / 1000.0,
                            peer.latency_tracker.stats().avg_rtt_us as f64 / 1000.0
                        );
                    }
                }
            }

            PKT_KEY_EXCHANGE => {
                let psk_ctx = CryptoContext::new(key, peer_mgr.session_id);
                let plaintext = psk_ctx.decrypt(
                    &header_bytes,
                    ciphertext,
                    header.peer_id,
                    header.seq_num,
                    header.counter,
                )?;
                let kx = KeyExchangePayload::from_bytes(&plaintext)?;

                let peer_id = header.peer_id;
                let local_id = peer_mgr.local_id;
                let session_id = peer_mgr.session_id;

                let derived_key =
                    key_exchange.derive_peer_key(&kx.public_key, &session_id, local_id, peer_id)?;

                if let Some(peer) = peer_mgr.get_peer_mut(peer_id) {
                    peer.peer_crypto = Some(CryptoContext::new(&derived_key, session_id));
                    peer.kx_received = true;
                    log::info!(
                        "Key exchange received from peer {peer_id} (PFS {})",
                        if peer.kx_sent { "active" } else { "pending" }
                    );
                }

                // Send our KEY_EXCHANGE back if we haven't yet
                let needs_send = peer_mgr
                    .get_peer(peer_id)
                    .map(|p| !p.kx_sent)
                    .unwrap_or(false);
                if needs_send {
                    let kx_payload = KeyExchangePayload {
                        public_key: key_exchange.public_key_bytes(),
                    };
                    let kx_bytes = kx_payload.to_bytes();
                    let hdr = PacketHeader::new(PKT_KEY_EXCHANGE, local_id, 0, 0);
                    let hdr_bytes = hdr.to_bytes();
                    let psk_ctx = CryptoContext::new(key, session_id);
                    if let Ok(encrypted) = psk_ctx.encrypt(&hdr_bytes, &kx_bytes, local_id, 0, 0) {
                        let wire = build_wire_packet(&hdr_bytes, &encrypted);
                        let _ = socket.send_to(&wire, src_addr);
                        if let Some(peer) = peer_mgr.get_peer_mut(peer_id) {
                            peer.kx_sent = true;
                            log::info!("Sent KEY_EXCHANGE to peer {peer_id} (PFS active)");
                        }
                    }
                }
            }

            _ => {
                return Err(anyhow!("unknown packet type: {}", header.pkt_type));
            }
        }

        Ok(())
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

/// Run a loopback test: capture -> [noise suppression] -> encode -> decode -> playout.
pub fn run_loopback(
    input_device: Option<&str>,
    output_device: Option<&str>,
    noise_suppression: bool,
    running: Arc<AtomicBool>,
) -> Result<()> {
    running.store(true, Ordering::SeqCst);

    let (capture_prod, mut capture_cons) = audio::create_audio_ring_buffer();
    let (mut playout_prod, playout_cons) = audio::create_audio_ring_buffer();

    let _capture = audio::start_capture(capture_prod, running.clone(), input_device)?;
    let _playout = audio::start_playout(playout_cons, running.clone(), output_device)?;

    let mut encoder = codec::Encoder::new()?;
    let mut decoder = codec::Decoder::new()?;
    let mut noise_suppressor = NoiseSuppressor::new();
    noise_suppressor.set_enabled(noise_suppression);

    let mut accum = Vec::with_capacity(FRAME_SAMPLES);
    let mut temp = [0.0f32; 4800];

    log::info!(
        "Loopback test running (noise suppression: {}, Ctrl+C to stop)",
        if noise_suppression { "on" } else { "off" }
    );

    while running.load(Ordering::Relaxed) {
        let read = capture_cons.pop_slice(&mut temp);
        if read > 0 {
            accum.extend_from_slice(&temp[..read]);
        }

        while accum.len() >= FRAME_SAMPLES {
            let frame_f32: Vec<f32> = accum.drain(..FRAME_SAMPLES).collect();

            let processed = noise_suppressor.process(&frame_f32);
            if let Some(denoised) = processed {
                for chunk in denoised.chunks(FRAME_SAMPLES) {
                    if chunk.len() < FRAME_SAMPLES {
                        break;
                    }
                    let frame_i16 = codec::f32_to_i16(chunk);
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
            }
        }

        std::thread::sleep(Duration::from_micros(FRAME_DURATION_US / 2));
    }

    Ok(())
}
