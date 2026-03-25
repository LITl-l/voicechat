# Ultra-Low-Latency Voice Chat — Architecture Design

## Project goals

- **Target**: Mouth-to-ear one-way latency ≤ 15ms (LAN), ≤ 25ms (same city)
- **Scale**: 5 simultaneous participants, full-mesh P2P
- **Platform**: Windows desktop (native binary)
- **Stack**: Rust + cpal + audiopus (libopus bindings) + raw UDP sockets

---

## 1. Latency budget (one-way)

| Stage | Aggressive | Conservative | Notes |
|---|---|---|---|
| Audio capture (WASAPI) | 2ms | 10ms | `BufferSize::Fixed(96)` @ 48kHz = 2ms. Shared mode OK on Win10+ |
| Opus encode | 5ms | 22.5ms | `RESTRICTED_LOWDELAY`, 2.5ms frame → 5ms algorithmic delay |
| Encrypt (XChaCha20-Poly1305) | ~0.0005ms | ~0.001ms | 80 bytes AEAD, negligible |
| Network transit (UDP) | <1ms (LAN) | 10-30ms | P2P direct, no relay |
| Decrypt + auth verify | ~0.0005ms | ~0.001ms | Reject invalid packets before decode |
| Jitter buffer | 0-3ms | 20-60ms | Adaptive, starts minimal |
| Opus decode | ~0.1ms | ~0.1ms | Negligible |
| Audio playout (WASAPI) | 2ms | 10ms | Same buffer config as capture |
| **Total** | **~10ms** | **~73ms** | Discord is estimated 60-150ms+ |

---

## 2. Network topology

### Full-mesh P2P (≤5 peers)

```
    A ←→ B
   /|╲   /|
  / | ╲ / |
 C ←→ D ←→ E
```

Each peer sends its encoded audio to every other peer individually.

- **Connections per peer**: N-1 (max 4)
- **Bandwidth per peer**: 4 streams × ~32-64 kbps = 128-256 kbps upload (trivial)
- **No mixing server**: Each client mixes received audio locally

### Connection establishment

1. One peer acts as **rendezvous** (shares its IP:port out-of-band, e.g., via clipboard)
2. New peers send a `JOIN` packet to rendezvous
3. Rendezvous responds with the list of all known peer addresses
4. Each peer sends `HELLO` directly to every other peer (UDP hole punching)
5. Peers exchange `HELLO_ACK` to confirm bidirectional connectivity
6. If direct P2P fails (symmetric NAT), fall back to relay through rendezvous peer

### NAT traversal

For the initial version: require port forwarding on at least one peer (the rendezvous).
Future: STUN-based hole punching via a lightweight public STUN server.

---

## 3. Packet format

Minimal custom header, no RTP (saves 12 bytes per packet, removes unnecessary complexity for 5-person private VC).

```
Offset  Size    Field
0       1       Type (0x01=Audio, 0x02=Join, 0x03=Hello, 0x04=Peer_List, 0x05=Keepalive)
1       1       Peer ID (0-4, assigned by rendezvous)
2       2       Sequence number (u16, wrapping)
4       4       Timestamp (u32, sample count at 48kHz, wraps every ~24h)
8       N       Opus payload (typ. 10-80 bytes for voice at 32-64kbps)
```

Total overhead: 8 bytes custom + 8 bytes UDP + 20 bytes IP = 36 bytes/packet.
Compare: RTP+UDP+IP = 40 bytes. Saving is small but every byte counts at 2.5ms frame intervals (400 pkt/s).

### Why not RTP?

- No RTCP needed (5 peers, we do our own keepalive/stats)
- No SSRC/CSRC needed (peer ID is sufficient)
- No payload type negotiation (always Opus)
- Simpler implementation, fewer dependencies

---

## 4. Audio pipeline

### Capture thread (high priority)

```
[Microphone] → WASAPI callback (2ms buffer, 48kHz mono f32)
            → Ring buffer (lock-free SPSC)
```

### Encode + encrypt thread

```
Ring buffer → Accumulate 2.5ms frame (120 samples @ 48kHz)
           → Opus encode (RESTRICTED_LOWDELAY, 48kHz, mono, 64kbps)
           → Prepend 8-byte header
           → XChaCha20-Poly1305 encrypt (header as AAD, payload encrypted, +16 byte tag)
           → sendto() × (N-1) peers via UDP socket
```

### Receive thread (per-socket, one thread)

```
recvfrom() → Verify auth tag (XChaCha20-Poly1305 decrypt)
           → Reject if auth fails (invalid/tampered/replay)
           → Parse header
           → Route to correct peer's decode buffer (by peer_id)
```

### Decode + mix thread

```
For each peer:
  Decode buffer → Opus decode → PCM f32 buffer
                              → (optional: PLC on missing packets)

Mix all peers: sum PCM samples, clamp to [-1.0, 1.0]
→ Ring buffer → WASAPI playout callback (2ms buffer)
```

### Thread model (4 threads total)

| Thread | Priority | Role |
|---|---|---|
| Audio capture | Real-time (MMCSS) | WASAPI input callback |
| Encode + Encrypt + Send | High | Opus encode, AEAD encrypt, UDP sendto |
| Receive + Decrypt | High | UDP recvfrom, AEAD verify+decrypt, route packets |
| Decode + Mix + Playout | Real-time (MMCSS) | Opus decode, mix, WASAPI output callback |

Inter-thread communication: lock-free SPSC ring buffers (`ringbuf` or `rtrb` crate).

---

## 5. Jitter buffer strategy

### Adaptive mini-buffer

For friends on known, stable connections, we can be extremely aggressive.

- **Initial depth**: 1 frame (2.5ms)
- **Max depth**: 4 frames (10ms)
- **Adaptation**: If >2% packets arrive late, increase by 1 frame. If <0.1% late for 5 seconds, decrease by 1 frame.
- **On missing packet**: Opus PLC (packet loss concealment) generates comfort audio. No silence insertion.
- **Out-of-order**: Accept if within 3 frames of expected sequence. Discard older.

### Zero-buffer mode (optional, LAN only)

Play audio immediately on arrival. No buffering at all. Works only with <1ms jitter.
Tradeoff: occasional glitch on jitter spike, but absolute minimum latency.

---

## 6. Security

### Threat model

| Threat | Severity | Mitigation |
|---|---|---|
| Eavesdropping (ISP/LAN sniffing raw Opus) | High | XChaCha20-Poly1305 AEAD on every audio packet |
| Packet injection (forge audio from fake peer) | High | Poly1305 auth tag — unauthenticated packets instantly dropped |
| Replay attack (resend captured valid packets) | High | Nonce derived from monotonic sequence number; reject old/duplicate seqs |
| MITM on key exchange | High | Pre-shared key (Phase 2-3), X25519 key exchange (Phase 4) |
| IP address disclosure (P2P exposes real IPs) | Medium | Accepted tradeoff — friends-only, known peers |
| UDP flood / DoS | Medium | Rate limit per source IP; auth tag check is fast (~ns), invalid packets dropped before decode |

### Encryption: XChaCha20-Poly1305

**Why this cipher?**

- Discord uses `aead_xchacha20_poly1305_rtpsize` as its mandatory voice encryption mode
- Software performance exceeds 1 GB/s on a single core — for 20-80 byte audio packets, cost is nanoseconds
- No AES-NI hardware dependency (ChaCha20 is fast on all CPUs)
- 192-bit nonce (XChaCha variant) allows safe nonce construction without collision risk
- Rust ecosystem: `chacha20poly1305` crate (RustCrypto, audited, well-maintained)

**Latency impact**: Encrypting/decrypting 80 bytes takes ~200-500 nanoseconds. At 400 packets/second,
total CPU time is <0.2ms/s. Completely invisible in the latency budget.

### Encrypted packet format

```
PLAINTEXT INPUT:
  [8-byte header]  [Opus payload, 10-80 bytes]

WIRE FORMAT:
  [8-byte header (cleartext, authenticated)] [encrypted Opus payload] [16-byte auth tag]

  Header fields (cleartext):
    0     1   Type
    1     1   Peer ID
    2     2   Sequence number (u16)
    4     4   Timestamp (u32)

  Encrypted region: Opus payload only
  Auth tag covers: header (as AAD) + encrypted payload
```

**Overhead**: +16 bytes per packet (auth tag only). Nonce is NOT transmitted — it is derived.

**Bandwidth impact**: At 400 pkt/s, +16 bytes = +6.4 kB/s = +51 kbps per stream.
With 4 outgoing streams: +204 kbps. Total per-peer upload rises from ~572 to ~776 kbps. Still trivial.

### Nonce construction

The 24-byte XChaCha20 nonce is derived deterministically — never transmitted on the wire:

```
nonce (24 bytes) = session_id (8 bytes)
                 || peer_id   (1 byte)
                 || direction (1 byte, 0x00=send, 0x01=recv)
                 || seq_num   (2 bytes, big-endian)
                 || counter   (4 bytes, monotonic per-peer, big-endian)
                 || zeros     (8 bytes, padding)
```

- `session_id`: random 8 bytes generated at session start, shared during handshake
- `direction`: prevents nonce reuse when two peers have the same seq_num
- `counter`: 32-bit monotonic counter (separate from the header seq_num which wraps at u16)
  — provides replay protection even after seq_num wraps

**Replay protection**: Receiver maintains a sliding window of accepted counter values per peer.
Packets with a counter older than `latest - 64` are rejected. Duplicate counters within the
window are rejected.

### Key exchange — phased approach

**Phase 2 (1:1) / Phase 3 (mesh): Pre-Shared Key (PSK)**

```
1. Users agree on a passphrase out-of-band (e.g., Discord DM, LINE, verbal)
2. Key derivation: Argon2id(passphrase, salt="voicechat-v1", t=3, m=64MB, p=1) → 256-bit key
3. The same key is used by all peers in the session
4. session_id is randomly generated by rendezvous and distributed in the PEER_LIST packet
   (PEER_LIST itself is encrypted with the PSK, bootstrapping the secure channel)
```

PSK is appropriate here because:
- Participants already know each other (friends)
- Group is small (≤5) — key distribution is trivial
- No PKI infrastructure needed
- Passphrase can be as simple as a game lobby code

**Phase 4 (future): X25519 ephemeral key exchange**

```
1. Each peer generates an X25519 keypair at session start
2. Public keys exchanged via TCP signaling channel (authenticated by PSK)
3. Shared secret derived per peer-pair via X25519 DH
4. Session key = HKDF-SHA256(shared_secret, session_id, "voicechat-audio")
5. Provides Perfect Forward Secrecy: compromised long-term PSK cannot decrypt past sessions
```

### Control packet encryption

Non-audio packets (JOIN, HELLO, PEER_LIST, KEEPALIVE) also need protection:

| Packet type | Encryption | Notes |
|---|---|---|
| JOIN | Encrypted with PSK | Proves the joiner knows the passphrase |
| HELLO | Encrypted with PSK | Mutual authentication between peers |
| HELLO_ACK | Encrypted with PSK | Confirms bidirectional secure channel |
| PEER_LIST | Encrypted with PSK | Contains session_id and peer addresses |
| KEEPALIVE | Encrypted with session key | Lightweight, just auth tag is sufficient |
| Audio | Encrypted with session key | Per-packet AEAD as described above |

### What we explicitly do NOT protect against

- **Traffic analysis**: An observer can see that UDP packets of consistent size are flowing
  between known IPs at 400 pkt/s. They can infer a voice call is happening, but cannot
  hear the content. Acceptable for a gaming VC tool.
- **Endpoint compromise**: If a peer's machine is compromised, the attacker has the key.
  This is true of all encryption systems. Out of scope.
- **IP address privacy**: P2P inherently reveals participant IPs. If this is unacceptable,
  a relay server is needed (which adds latency — counter to our goals).

---

## 7. Opus encoder configuration

```rust
let encoder = opus::Encoder::new(
    48000,                                  // sample rate
    opus::Channels::Mono,                   // mono (voice)
    opus::Application::RestrictedLowDelay,  // minimum algorithmic delay
)?;
encoder.set_bitrate(opus::Bitrate::Bits(64000))?;  // 64kbps for quality at small frames
encoder.set_inband_fec(true)?;                       // FEC for packet loss resilience
encoder.set_packet_loss_perc(5)?;                    // expect ~5% loss
// Frame size: 120 samples = 2.5ms at 48kHz
```

At 2.5ms frames with 64kbps, each packet payload is ~20 bytes.
400 packets/second × (20 + 36) bytes = ~17.9 kB/s = ~143 kbps per stream (including headers).
4 outgoing streams = ~572 kbps upload. Manageable.

Note: `RESTRICTED_LOWDELAY` forces CELT mode (no SILK). This is fine for
the use case — CELT handles voice well at 64kbps.

---

## 8. Crate dependencies

```toml
[dependencies]
cpal = "0.15"                # Audio I/O (WASAPI backend on Windows)
audiopus = "0.3"             # Opus codec bindings (or `opus` crate v0.3)
ringbuf = "0.4"              # Lock-free SPSC ring buffer
socket2 = "0.5"              # Low-level UDP sockets with SO_RCVTIMEO, SO_SNDBUF
crossbeam-utils = "0.8"      # Scoped threads, CachePadded
chacha20poly1305 = "0.10"   # XChaCha20-Poly1305 AEAD encryption
argon2 = "0.5"               # PSK key derivation (Argon2id)
rand = "0.8"                 # session_id generation, nonce randomness
```

### Optional / future

```toml
rnnoise-c = "0.1"       # Noise suppression (adds ~3ms latency)
x25519-dalek = "2"       # X25519 key exchange (Phase 4 PFS)
hkdf = "0.12"            # Key derivation from X25519 shared secret
clap = "4"               # CLI argument parsing
serde = "1"              # Config serialization
```

---

## 9. Cargo workspace layout

```
voicechat/
├── Cargo.toml              # workspace root
├── vc-core/
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs          # re-exports
│       ├── audio.rs        # cpal capture/playout, ring buffers
│       ├── codec.rs        # Opus encode/decode wrapper
│       ├── net.rs          # UDP socket, packet format, send/recv
│       ├── crypto.rs       # XChaCha20-Poly1305 encrypt/decrypt, nonce construction, PSK derivation
│       ├── jitter.rs       # Adaptive jitter buffer
│       ├── mixer.rs        # Multi-peer PCM mixing
│       └── peer.rs         # Peer state, connection management
├── vc-cli/
│   ├── Cargo.toml
│   └── src/
│       └── main.rs         # CLI entry point (headless mode)
└── vc-tray/                # Future: system tray GUI
    ├── Cargo.toml
    └── src/
        └── main.rs
```

---

## 10. MVP milestones

### Phase 1: Loopback test (1 week)
- Capture audio via cpal (WASAPI, small buffer)
- Encode with Opus (RESTRICTED_LOWDELAY, 2.5ms frames)
- Decode immediately
- Play back via cpal
- **Measure**: capture-to-playout latency with loopback cable

### Phase 2: 1:1 encrypted UDP voice (1-2 weeks)
- Send encoded audio over UDP to a hardcoded IP:port
- **XChaCha20-Poly1305 encryption from day one** (PSK via CLI argument)
- Nonce construction from session_id + peer_id + counter
- Replay protection with sliding window
- Receive, authenticate, decrypt, decode on the other end
- Implement basic jitter buffer (fixed 1-frame depth)
- **Measure**: end-to-end latency between two machines (with encryption)

### Phase 3: Multi-peer mesh (1-2 weeks)
- Peer discovery via rendezvous
- Full-mesh topology for up to 5 peers
- Encrypted control packets (JOIN/HELLO/PEER_LIST)
- Per-peer decode buffers
- Audio mixing
- Adaptive jitter buffer

### Phase 4: Polish (ongoing)
- Noise suppression (RNNoise)
- Voice activity detection (skip encoding silence → bandwidth savings)
- Push-to-talk / voice activation toggle
- System tray UI (vc-tray)
- Latency measurement overlay
- X25519 ephemeral key exchange for Perfect Forward Secrecy

---

## 11. Key design decisions & rationale

| Decision | Rationale |
|---|---|
| P2P full-mesh, no server | Zero relay hop = minimum latency for ≤5 peers |
| 2.5ms Opus frames | Smallest possible frame → lowest algorithmic delay (5ms) |
| RESTRICTED_LOWDELAY | Disables SILK, forces CELT. 15ms less delay than default |
| Custom packet header (not RTP) | 5-person private chat doesn't need RTP's complexity |
| WASAPI shared mode (not exclusive) | Game audio must still work. Win10+ shared mode ≥ 2ms buffer |
| Lock-free ring buffers | No mutex contention on audio threads |
| Mono audio | Voice doesn't need stereo. Halves bandwidth and processing |
| 48kHz sample rate | Opus native rate, avoids resampling |
| No noise gate by default | Adds latency. Push-to-talk preferred for FPS |
| XChaCha20-Poly1305 (not AES-GCM) | No AES-NI dependency, fast on all CPUs, Discord uses same cipher |
| Encryption from Phase 2 (not Phase 4) | Wi-Fi/ISP eavesdropping is real; +16 bytes and ~0ns latency cost is negligible |
| PSK before X25519 | Simpler for friends group; PFS can wait for Phase 4 |
| Nonce derived (not transmitted) | Saves 24 bytes/packet; deterministic from header + session state |

---

## 12. Risk assessment

| Risk | Mitigation |
|---|---|
| 2.5ms frames cause glitches on slow CPUs | Fall back to 5ms or 10ms frames. Config option |
| NAT traversal fails for some peers | Initial version requires 1 port-forwarded peer. STUN later |
| cpal WASAPI buffer can't go below 10ms on some hardware | Query `SupportedBufferSize` range, use minimum available |
| Opus at 2.5ms needs high bitrate for quality | 64kbps is fine. Can try 48kbps and A/B test |
| Multiple audio streams cause CPU contention with game | Decode is cheap (~0.01ms). Encode is the bottleneck: profile |
