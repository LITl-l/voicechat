/// Maximum jitter buffer depth in frames.
const MAX_DEPTH: usize = 4;
/// Initial jitter buffer depth in frames.
const INITIAL_DEPTH: usize = 1;
/// Late packet threshold to increase buffer (2%).
const INCREASE_THRESHOLD: f64 = 0.02;
/// Late packet threshold to decrease buffer (0.1%).
const DECREASE_THRESHOLD: f64 = 0.001;
/// Seconds of low-loss required before decreasing buffer.
const DECREASE_HOLDOFF_SECS: f64 = 5.0;

/// A single frame stored in the jitter buffer.
#[derive(Clone)]
struct JitterFrame {
    seq_num: u16,
    pcm: Vec<i16>,
}

/// Adaptive jitter buffer for a single peer.
pub struct JitterBuffer {
    /// Buffered frames, sorted by sequence number.
    frames: Vec<JitterFrame>,
    /// Current target depth in frames.
    target_depth: usize,
    /// Next expected sequence number for playout.
    next_playout_seq: Option<u16>,
    /// Stats for adaptation.
    total_packets: u64,
    late_packets: u64,
    /// Time since last depth decrease.
    time_since_decrease: f64,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            frames: Vec::with_capacity(MAX_DEPTH),
            target_depth: INITIAL_DEPTH,
            next_playout_seq: None,
            total_packets: 0,
            late_packets: 0,
            time_since_decrease: 0.0,
        }
    }

    /// Current buffer depth target (in frames).
    pub fn target_depth(&self) -> usize {
        self.target_depth
    }

    /// Number of frames currently buffered.
    pub fn buffered_frames(&self) -> usize {
        self.frames.len()
    }

    /// Push a decoded frame into the jitter buffer.
    pub fn push(&mut self, seq_num: u16, pcm: Vec<i16>) {
        self.total_packets += 1;

        if let Some(next) = self.next_playout_seq {
            let diff = seq_num.wrapping_sub(next);
            // If the packet is too old, count as late and discard
            if diff > u16::MAX / 2 {
                // Wrapped negative — packet is older than expected
                self.late_packets += 1;
                return;
            }
        }

        // Insert sorted by sequence number (ascending).
        // Find the first frame whose seq_num is greater than the new one.
        let pos = self
            .frames
            .iter()
            .position(|f| {
                // f.seq_num is "after" seq_num if f.seq_num - seq_num is small positive
                let diff = f.seq_num.wrapping_sub(seq_num);
                diff > 0 && diff < u16::MAX / 2
            })
            .unwrap_or(self.frames.len());

        // Don't insert duplicates
        if self.frames.iter().any(|f| f.seq_num == seq_num) {
            return;
        }

        // Don't exceed reasonable buffer size
        if self.frames.len() >= MAX_DEPTH * 2 {
            return;
        }

        self.frames.insert(pos, JitterFrame { seq_num, pcm });
    }

    /// Pop the next frame for playout.
    /// Returns None if the buffer hasn't reached target depth yet, or if the
    /// next expected frame isn't available (triggering PLC).
    pub fn pop(&mut self) -> Option<Vec<i16>> {
        // Wait until we have at least target_depth frames buffered
        if self.frames.is_empty() {
            return None;
        }

        if self.next_playout_seq.is_none() {
            // First playout: wait for target depth
            if self.frames.len() < self.target_depth {
                return None;
            }
            self.next_playout_seq = Some(self.frames[0].seq_num);
        }

        let expected = self.next_playout_seq.unwrap();

        // Check if the expected frame is in the buffer
        if let Some(idx) = self.frames.iter().position(|f| f.seq_num == expected) {
            let frame = self.frames.remove(idx);
            self.next_playout_seq = Some(expected.wrapping_add(1));
            Some(frame.pcm)
        } else {
            // Frame missing — advance sequence and signal PLC needed
            self.next_playout_seq = Some(expected.wrapping_add(1));
            None
        }
    }

    /// Call this periodically (e.g., once per frame interval) to adapt buffer depth.
    pub fn adapt(&mut self, elapsed_secs: f64) {
        self.time_since_decrease += elapsed_secs;

        if self.total_packets < 100 {
            return; // Not enough data to adapt
        }

        let loss_rate = self.late_packets as f64 / self.total_packets as f64;

        if loss_rate > INCREASE_THRESHOLD && self.target_depth < MAX_DEPTH {
            self.target_depth += 1;
            log::debug!(
                "jitter buffer increased to {} frames (loss rate: {:.2}%)",
                self.target_depth,
                loss_rate * 100.0
            );
            self.reset_stats();
        } else if loss_rate < DECREASE_THRESHOLD
            && self.target_depth > 1
            && self.time_since_decrease >= DECREASE_HOLDOFF_SECS
        {
            self.target_depth -= 1;
            self.time_since_decrease = 0.0;
            log::debug!(
                "jitter buffer decreased to {} frames (loss rate: {:.2}%)",
                self.target_depth,
                loss_rate * 100.0
            );
            self.reset_stats();
        }
    }

    fn reset_stats(&mut self) {
        self.total_packets = 0;
        self.late_packets = 0;
    }

    /// Reset the buffer (e.g., on peer reconnect).
    pub fn reset(&mut self) {
        self.frames.clear();
        self.next_playout_seq = None;
        self.target_depth = INITIAL_DEPTH;
        self.reset_stats();
        self.time_since_decrease = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::FRAME_SAMPLES;

    fn make_frame(seq: u16) -> Vec<i16> {
        vec![seq as i16; FRAME_SAMPLES]
    }

    #[test]
    fn test_basic_push_pop() {
        let mut jb = JitterBuffer::new();
        // Push one frame — target depth is 1, so it should be poppable
        jb.push(0, make_frame(0));
        let frame = jb.pop().unwrap();
        assert_eq!(frame.len(), FRAME_SAMPLES);
        assert_eq!(frame[0], 0);
    }

    #[test]
    fn test_out_of_order() {
        let mut jb = JitterBuffer::new();
        jb.push(1, make_frame(1));
        jb.push(0, make_frame(0));

        let f0 = jb.pop().unwrap();
        assert_eq!(f0[0], 0);
        let f1 = jb.pop().unwrap();
        assert_eq!(f1[0], 1);
    }

    #[test]
    fn test_missing_frame_returns_none() {
        let mut jb = JitterBuffer::new();
        jb.push(0, make_frame(0));
        let _ = jb.pop(); // seq 0

        // seq 1 is missing, push seq 2
        jb.push(2, make_frame(2));

        // Should return None for missing seq 1 (PLC signal)
        let result = jb.pop();
        assert!(result.is_none());

        // Now seq 2 should be available
        let f2 = jb.pop().unwrap();
        assert_eq!(f2[0], 2);
    }

    #[test]
    fn test_duplicate_ignored() {
        let mut jb = JitterBuffer::new();
        jb.push(0, make_frame(0));
        jb.push(0, make_frame(0)); // duplicate
        assert_eq!(jb.buffered_frames(), 1);
    }
}
