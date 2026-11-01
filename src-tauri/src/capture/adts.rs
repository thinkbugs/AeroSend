//! ADTS (ISO/IEC 14496-3) frame demuxer for ffmpeg's AAC output.
//!
//! AirPlay carries AAC-hbr without ADTS framing, so each frame's header is
//! stripped and only the raw access unit goes into the RTP payload.

/// Incremental ADTS demuxer over an encoder's byte stream.
#[derive(Default)]
pub struct AdtsParser {
    buffer: Vec<u8>,
}

/// A single AAC access unit. ffmpeg's encoder emits exactly 1024 samples per
/// frame, which is what the RTP timestamp advances by.
pub struct AdtsFrame {
    pub data: Vec<u8>,
    pub samples: usize,
}

pub const SAMPLES_PER_AAC_FRAME: usize = 1024;

/// frame_length spans the low 2 bits of byte 3, all of byte 4, and the high 3
/// bits of byte 5.
fn frame_length(hdr: &[u8]) -> usize {
    ((hdr[3] as usize & 0x03) << 11) | ((hdr[4] as usize) << 3) | ((hdr[5] as usize) >> 5)
}

impl AdtsParser {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Returns the next complete access unit, or `None` while more bytes are
    /// still in flight.
    pub fn next_frame(&mut self) -> Option<AdtsFrame> {
        loop {
            if self.buffer.len() < 7 {
                return None;
            }
            if self.buffer[0] != 0xFF || (self.buffer[1] & 0xF0) != 0xF0 {
                self.buffer.remove(0);
                continue;
            }

            let header_len = if self.buffer[1] & 0x01 == 0 { 9 } else { 7 };
            if self.buffer.len() < header_len {
                return None;
            }

            let total = frame_length(&self.buffer);
            if total < header_len {
                self.buffer.remove(0);
                continue;
            }
            if self.buffer.len() < total {
                return None;
            }

            let frame = self.buffer.drain(..total).as_slice().to_vec();
            return Some(AdtsFrame {
                data: frame[header_len..].to_vec(),
                samples: SAMPLES_PER_AAC_FRAME,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an ADTS header consistent with the field offsets `frame_length`
    /// reads. `protection_absent` selects the 7- vs 9-byte variant.
    fn header(payload_len: usize, channels: u8, protection_absent: bool) -> Vec<u8> {
        let header_len = if protection_absent { 7 } else { 9 };
        let total = payload_len + header_len;
        let mut h = vec![0u8; header_len];
        h[0] = 0xFF;
        h[1] = if protection_absent { 0xF1 } else { 0xF0 };
        h[2] = (1 << 6) | (4 << 2) | (channels >> 2); // AAC-LC, 44100Hz
        h[3] = ((channels & 3) << 6) | ((total >> 11) as u8 & 0x03);
        h[4] = (total >> 3) as u8;
        h[5] = ((total & 7) << 5) as u8 | 0x1F;
        h[6] = 0xF8;
        h
    }

    fn stream(frames: &[usize], protection_absent: bool) -> Vec<u8> {
        let mut raw = Vec::new();
        for (i, len) in frames.iter().enumerate() {
            raw.extend(header(*len, 2, protection_absent));
            raw.extend((0..*len).map(|b| (b + i) as u8));
        }
        raw
    }

    #[test]
    fn strips_the_seven_byte_header_and_keeps_the_access_unit() {
        let raw = stream(&[120], true);
        let mut p = AdtsParser::default();
        p.push(&raw);
        let frame = p.next_frame().expect("one frame");
        assert_eq!(frame.data, raw[7..]);
        assert_eq!(frame.samples, 1024);
        assert!(p.next_frame().is_none());
    }

    #[test]
    fn accounts_for_the_nine_byte_header_when_protection_is_present() {
        let raw = stream(&[64], false);
        let mut p = AdtsParser::default();
        p.push(&raw);
        let frame = p.next_frame().expect("frame");
        assert_eq!(frame.data.len(), 64);
        assert_eq!(frame.data, raw[9..]);
    }

    #[test]
    fn waits_for_a_frame_split_across_reads() {
        let raw = stream(&[300], true);
        let mut p = AdtsParser::default();
        p.push(&raw[..11]);
        assert!(p.next_frame().is_none());
        p.push(&raw[11..]);
        assert_eq!(p.next_frame().expect("completed frame").data.len(), 300);
    }

    #[test]
    fn demuxes_back_to_back_frames() {
        let raw = stream(&[100, 250, 40], true);
        let mut p = AdtsParser::default();
        p.push(&raw);
        let mut sizes = Vec::new();
        while let Some(f) = p.next_frame() {
            sizes.push(f.data.len());
        }
        assert_eq!(sizes, vec![100, 250, 40]);
    }

    #[test]
    fn resynchronises_past_leading_garbage() {
        let mut raw = vec![0x00, 0x11, 0x22, 0xFF, 0xE0, 0x01];
        raw.extend(stream(&[30], true));
        let good = stream(&[30], true);

        let mut p = AdtsParser::default();
        p.push(&raw);
        let frame = p.next_frame().expect("frame after resync");
        assert_eq!(frame.data, good[7..]);
    }

    #[test]
    fn handles_frames_large_enough_to_need_the_full_13_bit_length() {
        let raw = stream(&[1000, 4000, 8126], true);
        let mut p = AdtsParser::default();
        p.push(&raw);
        let mut sizes = Vec::new();
        while let Some(f) = p.next_frame() {
            sizes.push(f.data.len());
        }
        assert_eq!(sizes, vec![1000, 4000, 8126]);
    }
}
