//! RTP/RTCP packetization for H.264 (RFC 6184) and AAC-hbr.

use crate::airplay::h264::Nal;
use std::time::{SystemTime, UNIX_EPOCH};

/// Leave room for IP+UDP+RTP headers on an Ethernet path.
pub const RTP_MAX_PAYLOAD: usize = 1400;

const NAL_TYPE_FU_A: u8 = 28;

#[derive(Debug, Clone)]
pub struct RtpPacketizer {
    ssrc: u32,
    sequence_number: u16,
    timestamp: u32,
    clock_rate: u32,
    payload_type: u8,
}

impl RtpPacketizer {
    pub fn new(payload_type: u8, clock_rate: u32) -> Self {
        Self {
            ssrc: rand::random(),
            sequence_number: rand::random::<u16>() & 0x3F_FF,
            timestamp: rand::random::<u32>() & 0xFF_FFFF,
            clock_rate,
            payload_type,
        }
    }

    /// Converts one access unit into RTP packets sharing `timestamp`.
    ///
    /// The marker bit is set on the final packet so the receiver can submit the
    /// frame for decoding.
    pub fn packetize_h264(&mut self, nalus: &[Nal], timestamp: u32) -> Vec<Vec<u8>> {
        self.timestamp = timestamp;
        let mut payloads: Vec<Vec<u8>> = Vec::new();

        for nal in nalus {
            if nal.data.len() <= RTP_MAX_PAYLOAD {
                payloads.push(nal.data.clone());
            } else {
                payloads.extend(fragment_a(nal));
            }
        }

        let last = payloads.len() - 1;
        payloads
            .into_iter()
            .enumerate()
            .map(|(i, payload)| self.build_rtp_packet(&payload, i == last))
            .collect()
    }

    /// Packetizes one raw AAC access unit (no ADTS header) using AU-headers.
    /// Raw PCM payload: no codec framing, because L16 has none. The receiver
    /// reads fixed-size frames and advances its clock by their sample count.
    pub fn packetize_pcm(&mut self, samples: &[u8], timestamp: u32) -> Vec<u8> {
        self.timestamp = timestamp;
        self.build_rtp_packet(samples, true)
    }

    /// One audio payload with the marker bit under the caller's control:
    /// mirrored audio packets carry it clear, RAOP's L16 stream sets it.
    pub fn packetize_audio(&mut self, payload: &[u8], timestamp: u32, marker: bool) -> Vec<u8> {
        self.timestamp = timestamp;
        self.build_rtp_packet(payload, marker)
    }

    fn build_rtp_packet(&mut self, payload: &[u8], marker: bool) -> Vec<u8> {
        let mut packet = Vec::with_capacity(12 + payload.len());
        packet.push(0x80); // V=2, P=0, X=0, CC=0
        packet.push(if marker { 0x80 } else { 0 } | self.payload_type & 0x7F);
        packet.extend_from_slice(&self.sequence_number.to_be_bytes());
        self.sequence_number = self.sequence_number.wrapping_add(1);
        packet.extend_from_slice(&self.timestamp.to_be_bytes());
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    pub fn sequence_number(&self) -> u16 {
        self.sequence_number
    }

    pub fn timestamp(&self) -> u32 {
        self.timestamp
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// RTP clock ticks for a millisecond timestamp on this stream's clock.
    pub fn clock_for_ms(&self, elapsed_ms: u64) -> u32 {
        ((elapsed_ms as u64 * self.clock_rate as u64) / 1000) as u32
    }
}

/// RFC 6184 FU-A fragmentation.
fn fragment_a(nal: &Nal) -> Vec<Vec<u8>> {
    let indicator = (nal.data[0] & 0x60) | NAL_TYPE_FU_A;
    let nal_unit_type = nal.data[0] & 0x1F;
    let payload = &nal.data[1..];
    let chunk_size = RTP_MAX_PAYLOAD - 2;
    let fragment_count = payload.len().div_ceil(chunk_size);

    payload
        .chunks(chunk_size)
        .enumerate()
        .map(|(i, chunk)| {
            let mut header_byte = nal_unit_type;
            if i == 0 {
                header_byte |= 0x80; // S
            }
            if i == fragment_count - 1 {
                header_byte |= 0x40; // E
            }
            let mut p = Vec::with_capacity(chunk.len() + 2);
            p.push(indicator);
            p.push(header_byte);
            p.extend_from_slice(chunk);
            p
        })
        .collect()
}

/// RFC 3550: NTP timestamp, seconds since 1900-01-01 with 32-bit fraction.
pub fn current_nt_timestamp() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs() + 2_208_988_800;
    let frac = (duration.subsec_nanos() as u64) * 0x1_0000_0000 / 1_000_000_000;
    (secs << 32) | frac
}

#[derive(Debug)]
pub struct RtcpSenderReport {
    pub ssrc: u32,
    ntp_timestamp: u64,
    rtp_timestamp: u32,
    packet_count: u32,
    payload_octet_count: u32,
}

impl RtcpSenderReport {
    pub fn new(ssrc: u32) -> Self {
        Self {
            ssrc,
            ntp_timestamp: current_nt_timestamp(),
            rtp_timestamp: 0,
            packet_count: 0,
            payload_octet_count: 0,
        }
    }

    /// Tracks one sent packet's RTP timestamp and payload size.
    pub fn observe_packet(&mut self, rtp_ts: u32, payload_len: u32) {
        self.rtp_timestamp = rtp_ts;
        self.packet_count = self.packet_count.wrapping_add(1);
        self.payload_octet_count = self.payload_octet_count.wrapping_add(payload_len);
    }

    /// Stamps the wall-clock counterpart of the last observed RTP timestamp.
    pub fn mark_clock(&mut self) {
        self.ntp_timestamp = current_nt_timestamp();
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut packet = Vec::with_capacity(28);
        packet.push(0x80); // V=2, P=0, RC=0
        packet.push(200); // SR
        packet.extend_from_slice(&6u16.to_be_bytes()); // (28-4)/4
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        packet.extend_from_slice(&self.ntp_timestamp.to_be_bytes());
        packet.extend_from_slice(&self.rtp_timestamp.to_be_bytes());
        packet.extend_from_slice(&self.packet_count.to_be_bytes());
        packet.extend_from_slice(&self.payload_octet_count.to_be_bytes());
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::h264::NalType;

    fn nal(ty: u8, len: usize) -> Nal {
        let mut data = vec![ty];
        data.extend((0..len).map(|i| (i % 253) as u8 + 1));
        Nal { data }
    }

    #[test]
    fn small_nals_use_single_packet_mode() {
        let mut p = RtpPacketizer::new(96, 90000);
        let nalus = vec![nal(0x67, 20), nal(0x68, 4), nal(0x65, 300)];
        let packets = p.packetize_h264(&nalus, 1234);
        assert_eq!(packets.len(), 3);

        // header sanity on the first packet
        assert_eq!(packets[0][0], 0x80);
        assert_eq!(packets[0][1] & 0x7F, 96);
        assert_eq!(&packets[0][4..8], &1234u32.to_be_bytes());
        assert_eq!(&packets[0][8..12], &p.ssrc().to_be_bytes());

        // only the last packet of the access unit carries the marker
        assert_eq!(packets[0][1] & 0x80, 0);
        assert_eq!(packets[1][1] & 0x80, 0);
        assert_eq!(packets[2][1] & 0x80, 0x80);

        // payload is the untouched NAL
        assert_eq!(&packets[2][12..13], &[0x65]);
    }

    #[test]
    fn sequence_numbers_increase_across_packets_and_calls() {
        let mut p = RtpPacketizer::new(96, 90000);
        let first = p.sequence_number();
        let a = p.packetize_h264(&[nal(0x65, 2000)], 1);
        let b = p.packetize_h264(&[nal(0x61, 10)], 2);
        let used = a.len() + b.len();
        let seq_of = |pkt: &Vec<u8>| u16::from_be_bytes([pkt[2], pkt[3]]);
        assert_eq!(seq_of(&a[0]), first);
        assert_eq!(seq_of(&b[0]), first.wrapping_add(a.len() as u16));
        assert_eq!(p.sequence_number(), first.wrapping_add(used as u16));
    }

    #[test]
    fn large_nal_is_fragmented_with_fu_a_start_and_end() {
        let mut p = RtpPacketizer::new(96, 90000);
        let big = nal(0x65, RTP_MAX_PAYLOAD * 3 + 77);
        let packets = p.packetize_h264(&[big.clone()], 9_000);
        assert!(packets.len() >= 4);

        for (i, pkt) in packets.iter().enumerate() {
            let payload = &pkt[12..];
            let indicator = payload[0];
            let fu_header = payload[1];
            assert_eq!(indicator & 0x1F, NAL_TYPE_FU_A);
            assert_eq!(indicator & 0x60, 0x60, "NRI must be preserved");
            assert_eq!(fu_header & 0x1F, 5, "fu type carries the original nal type");
            assert_eq!(fu_header & 0x80 != 0, i == 0, "S bit only on first");
            assert_eq!(
                fu_header & 0x40 != 0,
                i == packets.len() - 1,
                "E bit only on last"
            );
        }

        // marker bit lands on the final fragment
        assert_eq!(packets.last().unwrap()[1] & 0x80, 0x80);

        // reassembling the fragments reproduces the original NAL byte-for-byte
        let mut rebuilt = vec![big.data[0]];
        for pkt in &packets {
            rebuilt.extend_from_slice(&pkt[14..]);
        }
        assert_eq!(rebuilt, big.data);
    }

    #[test]
    fn fragment_payload_never_exceeds_mtu() {
        let mut p = RtpPacketizer::new(96, 90000);
        let huge = nal(0x65, RTP_MAX_PAYLOAD * 20);
        let packets = p.packetize_h264(&[huge], 0);
        assert!(packets.iter().all(|pkt| pkt.len() <= 12 + RTP_MAX_PAYLOAD));
    }

    #[test]
    fn pcm_payload_is_sent_without_codec_framing() {
        let mut p = RtpPacketizer::new(96, 44100);
        let samples: Vec<u8> = (0..1408).map(|i| (i % 251) as u8).collect();
        let pkt = p.packetize_pcm(&samples, 500);
        assert_eq!(&pkt[0..1], &[0x80]);
        assert_eq!(pkt[1] & 0x7F, 96);
        assert_eq!(&pkt[12..], &samples[..]);
        assert_eq!(u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]), 500);
    }

    #[test]
    fn an_audio_packet_can_carry_the_marker_clear() {
        let mut p = RtpPacketizer::new(96, 44100);
        let pkt = p.packetize_audio(&[1, 2, 3], 7, false);
        assert_eq!(pkt[1], 0x60, "mirrored audio is payload type 96, marker 0");
        assert_eq!(&pkt[12..], &[1, 2, 3]);
    }

    #[test]
    fn a_pcm_timestamp_uses_the_streams_clock() {
        let mut p = RtpPacketizer::new(96, 44100);
        let pkt = p.packetize_pcm(&[1, 2, 3], p.clock_for_ms(1000));
        assert_eq!(u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]), 44100);
    }

    #[test]
    fn sender_report_counts_payload_octets_not_packet_size() {
        let mut sr = RtcpSenderReport::new(0x11223344);
        sr.observe_packet(1000, 500);
        sr.observe_packet(1001, 700);
        sr.mark_clock();
        let bytes = sr.serialize();
        assert_eq!(bytes.len(), 28);
        assert_eq!(bytes[1], 200);
        assert_eq!(&bytes[4..8], &0x11223344u32.to_be_bytes());
        assert_eq!(u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]), 1001);
        assert_eq!(u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]), 2);
        assert_eq!(u32::from_be_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]), 1200);
    }

    #[test]
    fn ntp_timestamp_uses_the_1900_epoch() {
        let nt = current_nt_timestamp();
        let secs = (nt >> 32) as u64;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(secs > now + 2_208_988_000 && secs < now + 2_208_989_600);
        assert!(nt as u32 > 0, "fraction must be populated");
    }

    #[test]
    fn nal_type_decodes_the_five_bit_type() {
        assert_eq!(NalType::from_header_byte(0x67), NalType::Sps);
        assert_eq!(NalType::from_header_byte(0x68), NalType::Pps);
        assert_eq!(NalType::from_header_byte(0x65), NalType::SliceIdr);
        assert_eq!(NalType::from_header_byte(0x41), NalType::SliceNonIdr);
        assert!(NalType::SliceIdr.is_slice());
        assert!(!NalType::Sps.is_slice());
    }
}
