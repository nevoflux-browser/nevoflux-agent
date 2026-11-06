//! One meeting segment's Opus packets as an Ogg Opus file (RFC 7845).
//!
//! A pendant sends Opus packets, one per `speech_chunk`; kept as they came,
//! an hour of a meeting is ~11 MB instead of the ~115 MB its PCM would be.
//! Ogg is the container a browser `<audio>` plays and every re-transcription
//! engine reads, so the packets are wrapped and never re-encoded.

/// Granule positions in Ogg Opus always count 48 kHz samples, whatever the
/// input rate was.
const GRANULE_RATE_PER_MS: u64 = 48;

/// Packets per page: about a second of 20 ms frames, so a page costs ~1% in
/// headers rather than the ~45% one page per packet would.
const PACKETS_PER_PAGE: usize = 50;

/// How long one Opus packet plays, in 48 kHz samples, read from its TOC byte
/// (RFC 6716 §3.1). `None` for an empty or malformed packet.
pub fn packet_samples_48k(packet: &[u8]) -> Option<u64> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    // Frame length in tenths of a millisecond.
    let frame_tenth_ms: u64 = match config {
        0..=11 => [100, 200, 400, 600][(config % 4) as usize],
        12..=15 => [100, 200][(config % 2) as usize],
        _ => [25, 50, 100, 200][(config % 4) as usize],
    };
    let frames: u64 = match toc & 0x03 {
        0 => 1,
        1 | 2 => 2,
        _ => u64::from(*packet.get(1)? & 0x3F),
    };
    if frames == 0 {
        return None;
    }
    Some(frames * frame_tenth_ms * GRANULE_RATE_PER_MS / 10)
}

/// The whole file for these packets. `input_rate` is what the encoder was fed
/// (16 kHz on the pendant); it is informational, players decode at 48 kHz.
pub fn encode(packets: &[Vec<u8>], input_rate: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let serial = 0x4E46_4D31; // "NFM1": any constant works for a one-stream file
    let mut page_no = 0u32;

    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(1); // channels
    head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&input_rate.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // mapping family: mono/stereo
    write_page(&mut out, serial, page_no, 0, 0x02, &[&head]);
    page_no += 1;

    let vendor = b"nevoflux";
    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // no comments
    write_page(&mut out, serial, page_no, 0, 0x00, &[&tags]);
    page_no += 1;

    let audio: Vec<&Vec<u8>> = packets
        .iter()
        .filter(|p| packet_samples_48k(p).is_some())
        .collect();
    let mut granule = 0u64;
    let chunks: Vec<_> = audio.chunks(PACKETS_PER_PAGE).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        for p in chunk.iter() {
            granule += packet_samples_48k(p).unwrap_or(0);
        }
        let flags = if i + 1 == chunks.len() { 0x04 } else { 0x00 };
        let slices: Vec<&[u8]> = chunk.iter().map(|p| p.as_slice()).collect();
        write_page(&mut out, serial, page_no, granule, flags, &slices);
        page_no += 1;
    }
    if chunks.is_empty() {
        write_page(&mut out, serial, page_no, 0, 0x04, &[]);
    }
    out
}

/// Total playing time of these packets, in milliseconds.
pub fn duration_ms(packets: &[Vec<u8>]) -> u64 {
    packets
        .iter()
        .filter_map(|p| packet_samples_48k(p))
        .sum::<u64>()
        / GRANULE_RATE_PER_MS
}

fn write_page(out: &mut Vec<u8>, serial: u32, page_no: u32, granule: u64, flags: u8, packets: &[&[u8]]) {
    let mut lacing = Vec::new();
    for p in packets {
        let mut n = p.len();
        while n >= 255 {
            lacing.push(255u8);
            n -= 255;
        }
        lacing.push(n as u8);
    }
    debug_assert!(lacing.len() <= 255, "a page holds at most 255 segments");

    let start = out.len();
    out.extend_from_slice(b"OggS");
    out.push(0); // version
    out.push(flags);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&page_no.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // CRC, filled below
    out.push(lacing.len() as u8);
    out.extend_from_slice(&lacing);
    for p in packets {
        out.extend_from_slice(p);
    }
    let crc = crc32(&out[start..]);
    out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
}

/// Ogg's CRC-32: polynomial 0x04C11DB7, not reflected, initial value 0.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CELT 20 ms mono frame (config 31, code 0), as the pendant encodes.
    fn frame(len: usize) -> Vec<u8> {
        let mut p = vec![0u8; len];
        p[0] = 31 << 3;
        p
    }

    #[test]
    fn toc_durations() {
        assert_eq!(packet_samples_48k(&frame(60)), Some(960)); // 20 ms
        assert_eq!(packet_samples_48k(&[(1 << 3) | 1, 0]), Some(1920)); // SILK 20 ms x2
        assert_eq!(packet_samples_48k(&[(16 << 3) | 3, 4]), Some(480)); // CELT 2.5 ms x4
        assert_eq!(packet_samples_48k(&[]), None);
        assert_eq!(packet_samples_48k(&[3]), None); // code 3 without a count byte
    }

    #[test]
    fn known_crc() {
        // CRC-32/CKSUM is this CRC with a final XOR of all ones; its check
        // value for "123456789" is 0x765E7680.
        assert_eq!(crc32(b"123456789"), 0x765E_7680 ^ 0xFFFF_FFFF);
    }

    #[test]
    fn file_layout() {
        let packets: Vec<Vec<u8>> = (0..120).map(|i| frame(40 + i % 300)).collect();
        let file = encode(&packets, 16000);
        // Head, tags, then 120 packets over three pages.
        let pages: Vec<usize> = file
            .windows(4)
            .enumerate()
            .filter(|(_, w)| *w == b"OggS")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(pages.len(), 5);
        assert_eq!(&file[28..36], b"OpusHead");
        // The last page carries end-of-stream and the full granule.
        let last = *pages.last().unwrap();
        assert_eq!(file[last + 5] & 0x04, 0x04);
        let g = u64::from_le_bytes(file[last + 6..last + 14].try_into().unwrap());
        assert_eq!(g, 120 * 960);
        assert_eq!(duration_ms(&packets), 2400);
        // Every page's CRC checks out.
        for (n, &at) in pages.iter().enumerate() {
            let end = pages.get(n + 1).copied().unwrap_or(file.len());
            let mut page = file[at..end].to_vec();
            let stored = u32::from_le_bytes(page[22..26].try_into().unwrap());
            page[22..26].copy_from_slice(&[0; 4]);
            assert_eq!(crc32(&page), stored, "page {n}");
        }
    }

    #[test]
    fn long_packet_lacing() {
        let file = encode(&[frame(600)], 16000);
        // 600 = 255 + 255 + 90: three lacing values on the audio page.
        let at = file.windows(4).enumerate().filter(|(_, w)| *w == b"OggS").nth(2).unwrap().0;
        assert_eq!(file[at + 26], 3);
        assert_eq!(&file[at + 27..at + 30], &[255, 255, 90]);
    }
}
