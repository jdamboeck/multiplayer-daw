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

/// Sample rates [`decode`] accepts. Outside them a file is more likely made up than recorded,
/// and converting it would take far more memory than the file suggests.
pub const RATES: std::ops::RangeInclusive<u32> = 8_000..=384_000;

/// Decodes FLAC to interleaved stereo. Mono becomes two equal channels; further channels are
/// dropped. A file longer than `max_secs` is an error: the bytes come from others, and a few of
/// them can claim, or hold, hours of silence. Measured in time, not frames, so that a low rate
/// does not let a long file through that grows large when converted to the output rate.
pub fn decode(bytes: &[u8], max_secs: u32) -> Result<Audio, FlacError> {
    let bad = |what: &str, e: claxon::Error| FlacError(format!("{what}: {e}"));
    let mut reader = claxon::FlacReader::new(bytes).map_err(|e| bad("not FLAC", e))?;
    let info = reader.streaminfo();
    if !RATES.contains(&info.sample_rate) {
        return Err(FlacError(format!("sample rate {} Hz", info.sample_rate)));
    }
    let max_frames = info.sample_rate as usize * max_secs as usize;
    let too_long = || FlacError(format!("longer than {max_secs} s"));
    if info.samples.is_some_and(|n| n > max_frames as u64) {
        return Err(too_long());
    }
    let channels = info.channels as usize;
    let scale = 1.0 / (1u64 << (info.bits_per_sample.clamp(1, 32) - 1)) as f32;
    let mut out = Vec::with_capacity(info.samples.unwrap_or(0) as usize * 2);
    let mut blocks = reader.blocks();
    let mut buffer = Vec::new();
    while let Some(block) = blocks.read_next_or_eof(buffer).map_err(|e| bad("decoding", e))? {
        if out.len() / 2 + block.duration() as usize > max_frames {
            return Err(too_long());
        }
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
        let audio = decode(&bytes, 1).unwrap();
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
    fn the_limit_is_a_length_in_time_at_any_rate() {
        // Three seconds at 8 kHz: few frames, but three seconds once at the output rate.
        let bytes = encode(&vec![0.1; 8_000 * 3 * 2], 8_000).unwrap();
        assert!(decode(&bytes, 2).is_err());
        assert_eq!(decode(&bytes, 3).unwrap().millis(), 3000);
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(decode(b"not a flac file at all", 1).is_err());
    }

    /// A short file whose STREAMINFO is changed by `patch`, which gets the 64 bits holding the
    /// sample rate, channels, bits per sample and total samples.
    fn patched(patch: impl Fn(u64) -> u64) -> Vec<u8> {
        let mut bytes = encode(&vec![0.1; 4800 * 2], 48_000).unwrap();
        // "fLaC", the block header, then 10 bytes of block and frame sizes.
        let at = 4 + 4 + 10;
        let field = u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap());
        bytes[at..at + 8].copy_from_slice(&patch(field).to_be_bytes());
        bytes
    }

    #[test]
    fn a_file_may_not_claim_or_hold_more_than_it_is_allowed() {
        let total = (1u64 << 36) - 1;
        let claims_hours = patched(|field| field | total);
        assert!(decode(&claims_hours, 1).is_err(), "no allocation for the claim");
        let says_nothing = patched(|field| field & !total);
        assert!(decode(&says_nothing, 1).is_ok());
        assert!(decode(&says_nothing, 0).is_err(), "counted while decoding");
        let one_hertz = patched(|field| (field & ((1 << 44) - 1)) | (1 << 44));
        assert!(
            decode(&one_hertz, 1).is_err(),
            "1 Hz would take 48000 times the memory"
        );
    }
}
