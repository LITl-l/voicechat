/// Mix multiple peer audio streams into a single output buffer.
///
/// Each input is a slice of f32 PCM samples in [-1.0, 1.0].
/// Output is the sum of all inputs, clamped to [-1.0, 1.0].
pub fn mix_peers(peer_buffers: &[&[f32]], output: &mut [f32]) {
    output.fill(0.0);
    for buf in peer_buffers {
        let len = buf.len().min(output.len());
        for i in 0..len {
            output[i] += buf[i];
        }
    }
    // Clamp to prevent clipping
    for sample in output.iter_mut() {
        *sample = sample.clamp(-1.0, 1.0);
    }
}

/// Mix peers with soft clipping (tanh) instead of hard clamp.
/// Sounds more natural when multiple people talk simultaneously.
pub fn mix_peers_soft(peer_buffers: &[&[f32]], output: &mut [f32]) {
    output.fill(0.0);
    for buf in peer_buffers {
        let len = buf.len().min(output.len());
        for i in 0..len {
            output[i] += buf[i];
        }
    }
    for sample in output.iter_mut() {
        *sample = sample.tanh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::FRAME_SAMPLES;

    #[test]
    fn test_mix_single_peer() {
        let peer = vec![0.5f32; FRAME_SAMPLES];
        let mut output = vec![0.0f32; FRAME_SAMPLES];
        mix_peers(&[&peer], &mut output);
        assert!((output[0] - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_mix_two_peers() {
        let p1 = vec![0.3f32; FRAME_SAMPLES];
        let p2 = vec![0.4f32; FRAME_SAMPLES];
        let mut output = vec![0.0f32; FRAME_SAMPLES];
        mix_peers(&[&p1, &p2], &mut output);
        assert!((output[0] - 0.7).abs() < 0.001);
    }

    #[test]
    fn test_mix_clamp() {
        let p1 = vec![0.8f32; FRAME_SAMPLES];
        let p2 = vec![0.8f32; FRAME_SAMPLES];
        let mut output = vec![0.0f32; FRAME_SAMPLES];
        mix_peers(&[&p1, &p2], &mut output);
        assert_eq!(output[0], 1.0);
    }

    #[test]
    fn test_mix_empty() {
        let mut output = vec![1.0f32; FRAME_SAMPLES];
        mix_peers(&[], &mut output);
        assert_eq!(output[0], 0.0);
    }
}
