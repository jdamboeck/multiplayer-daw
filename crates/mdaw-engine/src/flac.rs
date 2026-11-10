//! FLAC, the format of changelog samples: 16 bit stereo.

use std::fmt;

#[derive(Debug)]
pub struct FlacError(String);

impl fmt::Display for FlacError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for FlacError {}

/// Decoded audio: interleaved stereo.
#[derive(Clone, PartialEq, Debug)]
pub struct Audio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

impl Audio {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    /// Length in milliseconds.
    pub fn millis(&self) -> u32 {
        (self.frames() as u64 * 1000 / self.sample_rate.max(1) as u64) as u32
    }
}

/// Encodes interleaved stereo `f32` as 16-bit FLAC. Samples beyond full scale are clipped.
pub fn encode(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>, FlacError> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;

    let ints: Vec<i32> = samples
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i32)
        .collect();
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, e)| FlacError(format!("encoder setup: {e:?}")))?;
    let source = flacenc::source::MemSource::from_samples(&ints, 2, 16, sample_rate as usize);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| FlacError(format!("encoding: {e:?}")))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| FlacError(format!("writing: {e:?}")))?;
    Ok(sink.as_slice().to_vec())
}

/// Decodes FLAC to interleaved stereo. Mono becomes two equal channels; further channels are
/// dropped.
pub fn decode(bytes: &[u8]) -> Result<Audio, FlacError> {
    let bad = |what: &str, e: claxon::Error| FlacError(format!("{what}: {e}"));
    let mut reader = claxon::FlacReader::new(bytes).map_err(|e| bad("not FLAC", e))?;
    let info = reader.streaminfo();
    let channels = info.channels as usize;
    let scale = 1.0 / (1u64 << (info.bits_per_sample - 1)) as f32;
    let mut out = Vec::with_capacity(info.samples.unwrap_or(0) as usize * 2);
    let mut blocks = reader.blocks();
    let mut buffer = Vec::new();
    while let Some(block) = blocks.read_next_or_eof(buffer).map_err(|e| bad("decoding", e))? {
        for i in 0..block.duration() {
            let left = block.sample(0, i) as f32 * scale;
            let right = if channels > 1 {
                block.sample(1, i) as f32 * scale
            } else {
                left
            };
            out.push(left);
            out.push(right);
        }
        buffer = block.into_buffer();
    }
    Ok(Audio {
        samples: out,
        sample_rate: info.sample_rate,
    })
}

/// Converts interleaved stereo to another rate, by linear interpolation. Good enough for
/// listening to a changelog sample.
pub fn resample(audio: &Audio, rate: u32) -> Vec<f32> {
    if audio.sample_rate == rate || audio.frames() == 0 {
        return audio.samples.clone();
    }
    let frames_in = audio.frames();
    let frames_out = (frames_in as u64 * rate as u64 / audio.sample_rate as u64) as usize;
    let step = audio.sample_rate as f64 / rate as f64;
    let mut out = Vec::with_capacity(frames_out * 2);
    for i in 0..frames_out {
        let pos = i as f64 * step;
        let at = pos as usize;
        let frac = (pos - at as f64) as f32;
        let next = (at + 1).min(frames_in - 1);
        for ch in 0..2 {
            let a = audio.samples[at * 2 + ch];
            let b = audio.samples[next * 2 + ch];
            out.push(a + (b - a) * frac);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_survives_a_round_trip() {
        let samples: Vec<f32> = (0..48_000)
            .flat_map(|i| {
                let x = (i as f32 * 0.05).sin() * 0.5;
                [x, -x]
            })
            .collect();
        let bytes = encode(&samples, 48_000).unwrap();
        assert!(bytes.len() < samples.len() * 2, "FLAC compresses");
        let audio = decode(&bytes).unwrap();
        assert_eq!(audio.sample_rate, 48_000);
        assert_eq!(audio.samples.len(), samples.len());
        let worst = audio
            .samples
            .iter()
            .zip(&samples)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(worst < 1.0 / 16_000.0, "worst error {worst}");
        assert_eq!(audio.millis(), 1000);
    }

    #[test]
    fn resampling_keeps_the_length_in_time() {
        let audio = Audio {
            samples: vec![0.25; 48_000 * 2],
            sample_rate: 48_000,
        };
        let out = resample(&audio, 44_100);
        assert_eq!(out.len(), 44_100 * 2);
        assert!(out.iter().all(|s| (*s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(decode(b"not a flac file at all").is_err());
    }
}
