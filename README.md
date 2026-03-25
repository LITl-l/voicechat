# voicechat

Ultra-low-latency encrypted voice chat for small groups. Full-mesh P2P, no servers.

**Target**: mouth-to-ear latency ≤ 15ms (LAN), ≤ 25ms (same city) — compared to Discord's estimated 60-150ms+.

## Features

- **Opus codec** — `RESTRICTED_LOWDELAY` mode, 2.5ms frames, 48kHz mono, 64kbps
- **XChaCha20-Poly1305 encryption** — every packet authenticated and encrypted, ~0ns latency cost
- **Full-mesh P2P** — up to 5 participants, no relay server, minimal hops
- **Adaptive jitter buffer** — 1-4 frames (2.5-10ms), auto-tunes to network conditions
- **Lock-free audio pipeline** — SPSC ring buffers, no mutex contention on real-time threads
- **Replay protection** — sliding window filter per peer, rejects duplicate/old packets
- **PSK authentication** — Argon2id key derivation from shared passphrase

## Latency budget (one-way, aggressive)

| Stage | Time |
|---|---|
| Audio capture (WASAPI, 96-sample buffer) | 2ms |
| Opus encode (2.5ms frame, CELT mode) | 5ms |
| Encrypt (XChaCha20-Poly1305, 80 bytes) | ~0ms |
| Network transit (UDP, LAN) | <1ms |
| Decrypt + auth verify | ~0ms |
| Jitter buffer | 0-3ms |
| Opus decode | ~0ms |
| Audio playout | 2ms |
| **Total** | **~10ms** |

## Quick start

### Prerequisites

- Rust toolchain (stable)
- System dependencies: `libopus`, `alsa-lib` (Linux) or WASAPI (Windows)

With Nix:

```sh
nix develop
```

### Build

```sh
cargo build --release
```

### Usage

**List audio devices:**

```sh
cargo run -p vc-cli -- devices
```

**Host a session:**

```sh
cargo run -p vc-cli -- host -p "my-secret-passphrase"
```

**Join a session:**

```sh
cargo run -p vc-cli -- join 192.168.1.10:4567 -p "my-secret-passphrase"
```

**Loopback test** (capture → encode → decode → playout, no network):

```sh
cargo run -p vc-cli -- loopback
```

### Options

```
host  -b <bind_addr>  --input-device <name>  --output-device <name>  -p <passphrase>
join  <host:port>     --input-device <name>  --output-device <name>  -p <passphrase>  -b <bind_addr>
```

## Architecture

```
┌─────────────┐     ┌──────────────────┐     ┌─────────────┐
│  Microphone  │────▶│  SPSC Ring Buf   │────▶│ Opus Encode │
│  (cpal/WASAPI)│     │  (lock-free)     │     │ (2.5ms CELT)│
└─────────────┘     └──────────────────┘     └──────┬──────┘
                                                     │
                                          XChaCha20-Poly1305
                                                     │
                                               UDP sendto()
                                              ╱      │      ╲
                                         Peer A   Peer B   Peer C
                                              ╲      │      ╱
                                               UDP recvfrom()
                                                     │
                                          XChaCha20-Poly1305
                                                     │
                                              ┌──────┴──────┐
                                              │ Opus Decode  │
                                              │ + Jitter Buf │
                                              └──────┬──────┘
                                                     │
┌─────────────┐     ┌──────────────────┐     ┌──────┴──────┐
│   Speaker    │◀────│  SPSC Ring Buf   │◀────│  PCM Mixer  │
│  (cpal/WASAPI)│     │  (lock-free)     │     │ (sum+clamp) │
└─────────────┘     └──────────────────┘     └─────────────┘
```

### Packet format

8-byte custom header (no RTP overhead):

```
Offset  Size  Field
0       1     Type (0x01=Audio, 0x02=Join, 0x03=Hello, 0x04=PeerList, 0x05=Keepalive)
1       1     Peer ID (0-4)
2       2     Sequence number (u16, big-endian)
4       4     Timestamp (u32, sample count at 48kHz)
```

Wire format: `[header (8B, cleartext AAD)] [encrypted Opus payload] [auth tag (16B)]`

### Network topology

Full-mesh P2P for ≤5 peers. One peer acts as rendezvous (shares IP:port). New peers JOIN, receive PEER_LIST, then HELLO/HELLO_ACK with every other peer.

## Project structure

```
voicechat/
├── vc-core/        # Library: audio, codec, crypto, networking, peer management
├── vc-cli/         # CLI binary: host, join, loopback, devices
└── vc-tray/        # Future: system tray GUI
```

## Security

| Layer | Mechanism |
|---|---|
| Packet encryption | XChaCha20-Poly1305 AEAD (same cipher as Discord) |
| Key derivation | Argon2id (64MB, 3 iterations) from shared passphrase |
| Authentication | Poly1305 auth tag on every packet (header as AAD) |
| Replay protection | Per-peer sliding window (64 packets) |
| Nonce construction | Deterministic: `session_id ‖ peer_id ‖ direction ‖ seq ‖ counter` (never transmitted) |
| Control packets | All encrypted with PSK (JOIN, HELLO, PEER_LIST) |

## Roadmap

- [x] Phase 1: Loopback test (capture → encode → decode → playout)
- [x] Phase 2: 1:1 encrypted UDP voice
- [x] Phase 3: Multi-peer mesh with peer discovery
- [ ] Phase 4: Noise suppression (RNNoise), VAD, push-to-talk
- [ ] Phase 4: System tray GUI (`vc-tray`)
- [ ] Phase 4: X25519 ephemeral key exchange (Perfect Forward Secrecy)

## License

MIT
