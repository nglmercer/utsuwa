use crate::wav::encode_wav;

/// Interleaved signed 16-bit PCM collected from the input device.
pub struct PcmBuffer {
    samples: Vec<i16>,
    sample_rate: u32,
    channels: u16,
}

impl PcmBuffer {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            samples: Vec::new(),
            sample_rate,
            channels: channels.max(1),
        }
    }

    pub fn append(&mut self, samples: &[i16]) {
        self.samples.extend_from_slice(samples);
    }

    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 || self.channels == 0 {
            return 0;
        }
        let frames = self.samples.len() as u64 / self.channels as u64;
        frames.saturating_mul(1_000) / self.sample_rate as u64
    }

    pub fn into_wav(self) -> Vec<u8> {
        encode_wav(&self).unwrap_or_default()
    }

    /// Downmix the input and resample it to the STT interchange format.
    pub(crate) fn mono_16khz_samples(&self) -> Vec<i16> {
        let channels = self.channels.max(1) as usize;
        let frame_count = self.samples.len() / channels;
        if frame_count == 0 {
            return Vec::new();
        }

        let mut mono = Vec::with_capacity(frame_count);
        for frame in self.samples.chunks_exact(channels) {
            let sum: i64 = frame.iter().map(|sample| i64::from(*sample)).sum();
            let value = sum / channels as i64;
            mono.push(value.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16);
        }

        if self.sample_rate == 16_000 {
            return mono;
        }

        let output_len = ((mono.len() as u64)
            .saturating_mul(16_000)
            .saturating_add(self.sample_rate as u64 / 2)
            / self.sample_rate.max(1) as u64) as usize;
        if output_len == 0 {
            return Vec::new();
        }
        if mono.len() == 1 {
            return vec![mono[0]; output_len];
        }

        let scale = self.sample_rate as f64 / 16_000.0;
        let mut output = Vec::with_capacity(output_len);
        for index in 0..output_len {
            let source_position = index as f64 * scale;
            let left = source_position.floor() as usize;
            let left = left.min(mono.len() - 1);
            let right = (left + 1).min(mono.len() - 1);
            let fraction = source_position - left as f64;
            let value = mono[left] as f64 * (1.0 - fraction) + mono[right] as f64 * fraction;
            output.push(value.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_uses_interleaved_frames() {
        let mut pcm = PcmBuffer::new(48_000, 2);
        pcm.append(&vec![0; 48_000 * 2]);
        assert_eq!(pcm.duration_ms(), 1_000);
    }

    #[test]
    fn downmixes_and_resamples_to_mono_16khz() {
        let mut pcm = PcmBuffer::new(48_000, 2);
        pcm.append(&vec![1_000, 1_000].repeat(48_000));
        assert_eq!(pcm.mono_16khz_samples().len(), 16_000);
    }
}
