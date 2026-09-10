use crate::error::AudioError;
use crate::pcm::PcmBuffer;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::io::Cursor;

pub const OUTPUT_SAMPLE_RATE: u32 = 16_000;
pub const OUTPUT_CHANNELS: u16 = 1;

pub fn encode_wav(pcm: &PcmBuffer) -> Result<Vec<u8>, AudioError> {
    let samples = pcm.mono_16khz_samples();
    let spec = WavSpec {
        channels: OUTPUT_CHANNELS,
        sample_rate: OUTPUT_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::with_capacity(44 + samples.len() * 2));
    {
        let mut writer = WavWriter::new(&mut cursor, spec)
            .map_err(|error| AudioError::Wav(error.to_string()))?;
        for sample in samples {
            writer
                .write_sample(sample)
                .map_err(|error| AudioError::Wav(error.to_string()))?;
        }
        writer
            .finalize()
            .map_err(|error| AudioError::Wav(error.to_string()))?;
    }
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pcm::PcmBuffer;

    #[test]
    fn emits_pcm_16_bit_mono_wav() {
        let mut pcm = PcmBuffer::new(16_000, 1);
        pcm.append(&[0, 1_000, -1_000]);
        let wav = encode_wav(&pcm).unwrap();
        assert!(wav.starts_with(b"RIFF"));
        assert_eq!(&wav[22..24], 1u16.to_le_bytes().as_slice());
        assert_eq!(&wav[24..28], &16_000u32.to_le_bytes());
        assert_eq!(&wav[34..36], &16u16.to_le_bytes());
    }
}
