use std::collections::HashMap;
use std::time::Instant;

const MAX_PENDING_PINGS: usize = 16;

/// Latency statistics for a single peer.
#[derive(Clone, Debug, Default)]
pub struct LatencyStats {
    /// Last RTT measurement (microseconds).
    pub last_rtt_us: u64,
    /// Minimum RTT observed (microseconds).
    pub min_rtt_us: u64,
    /// Average RTT — exponential moving average (microseconds).
    pub avg_rtt_us: u64,
    /// Maximum RTT observed (microseconds).
    pub max_rtt_us: u64,
    /// Jitter — EMA of |rtt_n - rtt_{n-1}| (microseconds).
    pub jitter_us: u64,
    /// Total measurements taken.
    pub sample_count: u64,
}

/// Per-peer latency tracker using ping/pong round-trip measurement.
pub struct LatencyTracker {
    pending: HashMap<u32, Instant>,
    next_ping_id: u32,
    stats: LatencyStats,
    /// EMA smoothing factor (same as TCP: 1/8).
    alpha: f64,
    last_rtt: f64,
}

impl Default for LatencyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyTracker {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            next_ping_id: 0,
            stats: LatencyStats {
                min_rtt_us: u64::MAX,
                ..Default::default()
            },
            alpha: 0.125,
            last_rtt: 0.0,
        }
    }

    /// Record a ping sent. Returns the ping_id to include in the packet.
    pub fn ping_sent(&mut self) -> u32 {
        let id = self.next_ping_id;
        self.next_ping_id = self.next_ping_id.wrapping_add(1);

        if self.pending.len() >= MAX_PENDING_PINGS {
            if let Some(&oldest) = self.pending.keys().next() {
                self.pending.remove(&oldest);
            }
        }

        self.pending.insert(id, Instant::now());
        id
    }

    /// Record a pong received. Returns RTT in microseconds if the ping_id is known.
    pub fn pong_received(&mut self, ping_id: u32) -> Option<u64> {
        let send_time = self.pending.remove(&ping_id)?;
        let rtt_us = send_time.elapsed().as_micros() as u64;

        self.stats.last_rtt_us = rtt_us;
        self.stats.sample_count += 1;

        if rtt_us < self.stats.min_rtt_us {
            self.stats.min_rtt_us = rtt_us;
        }
        if rtt_us > self.stats.max_rtt_us {
            self.stats.max_rtt_us = rtt_us;
        }

        let rtt_f = rtt_us as f64;
        if self.stats.sample_count == 1 {
            self.stats.avg_rtt_us = rtt_us;
            self.stats.jitter_us = 0;
        } else {
            let avg = self.stats.avg_rtt_us as f64;
            self.stats.avg_rtt_us = (avg * (1.0 - self.alpha) + rtt_f * self.alpha) as u64;

            let deviation = (rtt_f - self.last_rtt).abs();
            let jitter = self.stats.jitter_us as f64;
            self.stats.jitter_us = (jitter * (1.0 - self.alpha) + deviation * self.alpha) as u64;
        }
        self.last_rtt = rtt_f;

        Some(rtt_us)
    }

    pub fn stats(&self) -> &LatencyStats {
        &self.stats
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        self.stats = LatencyStats {
            min_rtt_us: u64::MAX,
            ..Default::default()
        };
        self.last_rtt = 0.0;
    }
}

/// Payload for PING and PONG packets (4 bytes).
#[derive(Clone, Debug)]
pub struct PingPayload {
    pub ping_id: u32,
}

impl PingPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        self.ping_id.to_be_bytes().to_vec()
    }

    pub fn from_bytes(data: &[u8]) -> anyhow::Result<Self> {
        if data.len() < 4 {
            return Err(anyhow::anyhow!("PING payload too short"));
        }
        Ok(Self {
            ping_id: u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ping_pong_rtt() {
        let mut tracker = LatencyTracker::new();
        let id = tracker.ping_sent();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let rtt = tracker.pong_received(id);
        assert!(rtt.is_some());
        assert!(rtt.unwrap() >= 500); // at least ~0.5ms
        assert_eq!(tracker.stats().sample_count, 1);
    }

    #[test]
    fn test_unknown_pong() {
        let mut tracker = LatencyTracker::new();
        assert!(tracker.pong_received(999).is_none());
    }

    #[test]
    fn test_ping_payload_roundtrip() {
        let payload = PingPayload { ping_id: 42 };
        let bytes = payload.to_bytes();
        let parsed = PingPayload::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.ping_id, 42);
    }

    #[test]
    fn test_stats_update() {
        let mut tracker = LatencyTracker::new();
        for _ in 0..5 {
            let id = tracker.ping_sent();
            tracker.pong_received(id);
        }
        assert_eq!(tracker.stats().sample_count, 5);
        assert!(tracker.stats().min_rtt_us <= tracker.stats().avg_rtt_us);
        assert!(tracker.stats().avg_rtt_us <= tracker.stats().max_rtt_us);
    }
}
