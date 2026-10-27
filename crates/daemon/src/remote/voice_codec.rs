//! Spoken replies re-encoded for a device on a thin link.
//!
//! A sentence leaves the synthesiser as a base64 WAV at the engine's rate
//! (24 kHz mono 16-bit ≈ 64 KB/s once base64'd). Over a phone hotspot and the
//! relay that is slower than real time, and the pendant plays it at 16 kHz
//! anyway. A device that says it decodes Opus (`voice_codec: "opus"` on its
//! `user_message`) gets each `voice_audio` frame as 16 kHz mono Opus instead:
//! 20 ms packets at 24 kbit/s, about 4 KB/s base64'd — some 16x smaller.
//!
//! Wire shape, replacing `wav`:
//!
//! ```json
//! { "kind": "voice_audio", "turn_id": "…", "seq": 0, "sample_rate": 16000,
//!   "codec": "opus", "frame_ms": 20, "opus": "<base64>" }
//! ```
//!
//! `opus` is the sentence's packets back to back, each prefixed by its length
//! as a little-endian u16. Every sentence is encoded by a fresh encoder, so a
//! player can decode one without having heard the one before.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;

/// The rate the pendant's speaker and microphone share.
pub const OPUS_SAMPLE_RATE: u32 = 16_000;
pub const OPUS_FRAME_MS: u32 = 20;
const FRAME_SAMPLES: usize = (OPUS_SAMPLE_RATE * OPUS_FRAME_MS / 1000) as usize;
/// Wideband speech is transparent well below this; music is not the job.
const BITRATE_BPS: i32 = 24_000;
/// The largest packet Opus emits.
const MAX_PACKET: usize = 1275;

/// Re-encode a `voice_audio` frame in place. False, with the frame untouched,
/// when it is not one or its WAV cannot be read — the device then gets the WAV.
pub fn transcode_voice_audio(frame: &mut Value) -> bool {
    if frame.get("kind").and_then(Value::as_str) != Some("voice_audio") {
        return false;
    }
    let Some(wav_b64) = frame.get("wav").and_then(Value::as_str) else {
        return false;
    };
    let Ok(wav) = STANDARD.decode(wav_b64) else {
        return false;
    };
    let Some((pcm, rate)) = parse_wav(&wav) else {
        return false;
    };
    let pcm = resample(&pcm, rate, OPUS_SAMPLE_RATE);
    let Ok(packets) = encode_opus(&pcm) else {
        return false;
    };
    let Some(obj) = frame.as_object_mut() else {
        return false;
    };
    obj.remove("wav");
    obj.insert("sample_rate".into(), OPUS_SAMPLE_RATE.into());
    obj.insert("codec".into(), "opus".into());
    obj.insert("frame_ms".into(), OPUS_FRAME_MS.into());
    obj.insert("opus".into(), STANDARD.encode(packets).into());
    true
}

/// Mono samples in [-1, 1] and the rate of a PCM WAV: 16-bit integer or 32-bit
/// float, any channel count (mixed down).
pub fn parse_wav(bytes: &[u8]) -> Option<(Vec<f32>, u32)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut at = 12usize;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32_at(bytes, at + 4) as usize;
        let body = at + 8;
        let end = body.saturating_add(len).min(bytes.len());
        if id == b"fmt " && end - body >= 16 {
            fmt = Some((
                u16_at(bytes, body),
                u16_at(bytes, body + 2),
                u32_at(bytes, body + 4),
                u16_at(bytes, body + 14),
            ));
        } else if id == b"data" {
            let (format, channels, rate, bits) = fmt?;
            let channels = channels as usize;
            if channels == 0 || rate == 0 {
                return None;
            }
            let data = &bytes[body..end];
            let samples: Vec<f32> = match (format, bits) {
                (1, 16) => data
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                    .collect(),
                (3, 32) => data
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
                _ => return None,
            };
            let mono = samples
                .chunks_exact(channels)
                .map(|f| f.iter().sum::<f32>() / channels as f32)
                .collect();
            return Some((mono, rate));
        }
        // Chunks are padded to an even length.
        at = body.saturating_add(len + (len & 1));
    }
    None
}

/// Band-limited resampling by windowed sinc (Blackman, 16 zero crossings a
/// side). The cutoff follows the lower of the two Nyquist rates, so going down
/// from 24 kHz removes what 16 kHz cannot hold instead of folding it back.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    const ZERO_CROSSINGS: f64 = 16.0;
    let step = from as f64 / to as f64;
    let cutoff = (to as f64 / from as f64).min(1.0);
    let half = (ZERO_CROSSINGS / cutoff).ceil() as isize;
    let out_len = ((input.len() as u64 * to as u64).div_ceil(from as u64)) as usize;
    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let t = n as f64 * step;
        let centre = t.floor() as isize;
        let mut acc = 0.0f64;
        for k in (centre - half + 1)..=(centre + half) {
            if k < 0 || k as usize >= input.len() {
                continue;
            }
            let d = t - k as f64;
            let x = d / half as f64; // -1..1 across the window
            if x.abs() >= 1.0 {
                continue;
            }
            let window = 0.42
                + 0.5 * (std::f64::consts::PI * x).cos()
                + 0.08 * (2.0 * std::f64::consts::PI * x).cos();
            let arg = cutoff * d;
            let sinc = if arg.abs() < 1e-12 {
                1.0
            } else {
                (std::f64::consts::PI * arg).sin() / (std::f64::consts::PI * arg)
            };
            acc += input[k as usize] as f64 * cutoff * sinc * window;
        }
        out.push(acc as f32);
    }
    out
}

/// 16 kHz mono samples as length-prefixed 20 ms Opus packets. The tail is
/// padded with silence, one frame beyond the end, so the encoder's look-ahead
/// does not swallow the last syllable.
pub fn encode_opus(pcm: &[f32]) -> Result<Vec<u8>, String> {
    use unsafe_libopus::{
        opus_encode, opus_encoder_create, opus_encoder_ctl, opus_encoder_destroy,
        OPUS_APPLICATION_VOIP, OPUS_OK, OPUS_SET_BITRATE_REQUEST, OPUS_SET_COMPLEXITY_REQUEST,
        OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE,
    };

    let frames = pcm.len().div_ceil(FRAME_SAMPLES) + 1;
    let mut samples: Vec<i16> = pcm
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect();
    samples.resize(frames * FRAME_SAMPLES, 0);

    let mut out = Vec::with_capacity(frames * 64);
    let mut packet = [0u8; MAX_PACKET];
    // SAFETY: the encoder is created, used and destroyed in this scope only;
    // every buffer passed in outlives the call and is the size given with it.
    unsafe {
        let mut err = 0i32;
        let enc = opus_encoder_create(
            OPUS_SAMPLE_RATE as i32,
            1,
            OPUS_APPLICATION_VOIP,
            &mut err,
        );
        if enc.is_null() || err != OPUS_OK {
            return Err(format!("opus_encoder_create failed: {err}"));
        }
        opus_encoder_ctl!(enc, OPUS_SET_BITRATE_REQUEST, BITRATE_BPS);
        opus_encoder_ctl!(enc, OPUS_SET_COMPLEXITY_REQUEST, 10);
        opus_encoder_ctl!(enc, OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE);
        let mut result = Ok(());
        for frame in samples.chunks_exact(FRAME_SAMPLES) {
            let n = opus_encode(
                enc,
                frame.as_ptr(),
                FRAME_SAMPLES as i32,
                packet.as_mut_ptr(),
                MAX_PACKET as i32,
            );
            if n < 0 {
                result = Err(format!("opus_encode failed: {n}"));
                break;
            }
            out.extend_from_slice(&(n as u16).to_le_bytes());
            out.extend_from_slice(&packet[..n as usize]);
        }
        opus_encoder_destroy(enc);
        result?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wav16(pcm: &[f32], rate: u32) -> Vec<u8> {
        let data_len = (pcm.len() * 2) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for &s in pcm {
            out.extend_from_slice(&((s * 32767.0) as i16).to_le_bytes());
        }
        out
    }

    fn tone(freq: f32, rate: u32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    /// Amplitude of `freq` in `x` (single-bin DFT), 0.5 for the test tone.
    fn amplitude_at(x: &[f32], freq: f32, rate: u32) -> f32 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in x.iter().enumerate() {
            let ph = 2.0 * std::f64::consts::PI * freq as f64 * i as f64 / rate as f64;
            re += s as f64 * ph.cos();
            im += s as f64 * ph.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / x.len() as f64) as f32
    }

    fn decode_packets(bytes: &[u8]) -> Vec<f32> {
        use unsafe_libopus::{opus_decode, opus_decoder_create, opus_decoder_destroy};
        let mut out = Vec::new();
        unsafe {
            let mut err = 0;
            let dec = opus_decoder_create(OPUS_SAMPLE_RATE as i32, 1, &mut err);
            assert!(!dec.is_null());
            let mut at = 0;
            let mut pcm = [0i16; FRAME_SAMPLES];
            while at + 2 <= bytes.len() {
                let len = u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
                at += 2;
                let n = opus_decode(
                    dec,
                    bytes[at..at + len].as_ptr(),
                    len as i32,
                    pcm.as_mut_ptr(),
                    FRAME_SAMPLES as i32,
                    0,
                );
                assert_eq!(n, FRAME_SAMPLES as i32);
                out.extend(pcm.iter().map(|&s| s as f32 / 32768.0));
                at += len;
            }
            assert_eq!(at, bytes.len(), "packets fill the payload exactly");
            opus_decoder_destroy(dec);
        }
        out
    }

    #[test]
    fn resampling_keeps_a_tone_and_its_length() {
        let x = tone(440.0, 24_000, 1.0);
        let y = resample(&x, 24_000, 16_000);
        assert_eq!(y.len(), 16_000);
        let a = amplitude_at(&y[1000..15000], 440.0, 16_000);
        assert!((a - 0.5).abs() < 0.01, "amplitude {a}");
    }

    #[test]
    fn resampling_removes_what_the_lower_rate_cannot_hold() {
        // 10 kHz is above 16 kHz's Nyquist: it must go, not alias to 6 kHz.
        let x = tone(10_000.0, 24_000, 0.5);
        let y = resample(&x, 24_000, 16_000);
        assert!(amplitude_at(&y[500..7500], 6_000.0, 16_000) < 0.01);
    }

    #[test]
    fn a_wav_frame_becomes_opus_a_tenth_the_size_and_still_sounds_the_same() {
        let speechy = tone(300.0, 24_000, 3.0);
        let wav_b64 = STANDARD.encode(wav16(&speechy, 24_000));
        let mut frame = json!({
            "kind": "voice_audio", "turn_id": "vt_1", "seq": 0,
            "sample_rate": 24_000, "wav": wav_b64,
        });
        assert!(transcode_voice_audio(&mut frame));
        assert!(frame.get("wav").is_none());
        assert_eq!(frame["codec"], "opus");
        assert_eq!(frame["sample_rate"], 16_000);
        assert_eq!(frame["frame_ms"], 20);
        assert_eq!(frame["turn_id"], "vt_1");
        let opus_b64 = frame["opus"].as_str().unwrap();
        assert!(
            opus_b64.len() * 10 < wav_b64.len(),
            "opus {} vs wav {}",
            opus_b64.len(),
            wav_b64.len()
        );

        let pcm = decode_packets(&STANDARD.decode(opus_b64).unwrap());
        // 3 s, rounded up to whole frames, plus the flush frame.
        assert_eq!(pcm.len(), (150 + 1) * FRAME_SAMPLES);
        let a = amplitude_at(&pcm[1600..46400], 300.0, 16_000);
        assert!((a - 0.5).abs() < 0.1, "decoded amplitude {a}");
    }

    #[test]
    fn stereo_and_float_wavs_are_read() {
        // 2-channel 16-bit: left 0.5, right -0.5 → mono 0.
        let mut w = wav16(&[0.5, -0.5, 0.5, -0.5], 16_000);
        w[22] = 2; // channels
        let (mono, rate) = parse_wav(&w).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(mono.len(), 2);
        assert!(mono.iter().all(|s| s.abs() < 1e-3));
    }

    #[test]
    fn anything_else_is_left_alone() {
        let mut done = json!({ "kind": "voice_done", "turn_id": "t", "spoken": 1 });
        let before = done.clone();
        assert!(!transcode_voice_audio(&mut done));
        assert_eq!(done, before);

        let mut broken = json!({ "kind": "voice_audio", "wav": "bm90IGEgd2F2" });
        let before = broken.clone();
        assert!(!transcode_voice_audio(&mut broken));
        assert_eq!(broken, before);
    }
}
