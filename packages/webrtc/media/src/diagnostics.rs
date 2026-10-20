//! Offline codec fixture for native device acceptance; no microphone, camera or model.
use crate::{AudioProcessor, video::VideoCodec};
use std::time::{Duration, Instant};
pub fn verify_codecs() -> Result<u32, Box<dyn std::error::Error>> {
    let mut encoder = VideoCodec::encoder(64, 48, 15, 256000)?;
    let mut decoder = VideoCodec::decoder(64, 48)?;
    let layout = encoder.layout().clone();
    for index in 0..5u64 {
        let mut pixels = vec![128; layout.size];
        for row in 0..48 {
            let offset = layout.offsets[0] + row * layout.strides[0] as usize;
            pixels[offset..offset + 64].fill(70 + index as u8 * 10);
        }
        let pts = Duration::from_millis(index * 67);
        let duration = Duration::from_millis(67);
        let encoded = encoder.process(pixels, pts, duration)?;
        let decoded = decoder.process(encoded, pts, duration)?;
        let average = decoded[..64].iter().map(|n| f64::from(*n)).sum::<f64>() / 64.;
        if decoded.len() != layout.size || (average - (70 + index * 10) as f64).abs() > 6. {
            return Err("VP8 decoded frame differs".into());
        }
    }
    let mut processor = AudioProcessor::new(48000, 48000)?;
    processor.validate_callback_timing(256, 256)?;
    let mut opus_decoder = opus::Decoder::new(48000, opus::Channels::Mono)?;
    let mut decoded = [0.; 5760];
    let start = Instant::now();
    let mut packets = 0;
    let mut energy = 0.;
    for index in 0..128 {
        let at = start + Duration::from_secs_f64((index * 256) as f64 / 48000.);
        let now = Instant::now();
        if at > now {
            std::thread::sleep(at - now);
        }
        let samples: Vec<_> = (0..256)
            .map(|n| (((index * 256 + n) as f32) * 0.07).sin() * 0.2)
            .collect();
        for packet in processor.capture(&samples, at)? {
            let count = opus_decoder.decode_float(&packet.data, &mut decoded, false)?;
            if count != 960 {
                return Err("Opus packet is not 20 ms".into());
            }
            energy += decoded[..count].iter().map(|n| n * n).sum::<f32>();
            packets += 1;
        }
    }
    if packets < 10 || energy <= 0.01 {
        return Err("shared audio processing produced no signal".into());
    }
    Ok(packets)
}
