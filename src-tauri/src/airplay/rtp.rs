use std::time::{SystemTime, UNIX_EPOCH};
use tracing::debug;

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
            sequence_number: rand::random(),
            timestamp: rand::random::<u32>() & 0xFFFF_FFFF,
            clock_rate,
            payload_type,
        }
    }

    pub fn packetize_h264(&mut self, nalu: &[u8], pts_ms: u64) -> Vec<u8> {
        self.timestamp = ((pts_ms as f64 / 1000.0) * self.clock_rate as f64) as u32;
        self.build_rtp_packet(nalu)
    }

    pub fn packetize_aac(&mut self, frame: &[u8], pts_ms: u64) -> Vec<u8> {
        self.timestamp = ((pts_ms as f64 / 1000.0) * self.clock_rate as f64) as u32;
        let au_headers = build_aac_au_headers(frame.len());
        let mut payload = Vec::with_capacity(au_headers.len() + frame.len());
        payload.extend_from_slice(&au_headers);
        payload.extend_from_slice(frame);
        self.build_rtp_packet(&payload)
    }

    fn build_rtp_packet(&mut self, payload: &[u8]) -> Vec<u8> {
        let mut packet = Vec::with_capacity(12 + payload.len());

        // Byte 0: V=2, P=0, X=0, CC=0
        packet.push(0x80);

        // Byte 1: M=0, PT
        let m_bit = 0u8;
        packet.push((m_bit << 7) | (self.payload_type & 0x7F));

        // Bytes 2-3: Sequence number (big-endian)
        packet.push((self.sequence_number >> 8) as u8);
        packet.push(self.sequence_number as u8);
        self.sequence_number = self.sequence_number.wrapping_add(1);

        // Bytes 4-7: Timestamp (big-endian)
        packet.push((self.timestamp >> 24) as u8);
        packet.push((self.timestamp >> 16) as u8);
        packet.push((self.timestamp >> 8) as u8);
        packet.push(self.timestamp as u8);

        // Bytes 8-11: SSRC (big-endian)
        packet.push((self.ssrc >> 24) as u8);
        packet.push((self.ssrc >> 16) as u8);
        packet.push((self.ssrc >> 8) as u8);
        packet.push(self.ssrc as u8);

        // Payload
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
}

fn build_aac_au_headers(frame_len: usize) -> Vec<u8> {
    // AU-headers-length (2 bytes) + AU header (2 bytes per AU)
    let headers_length: u16 = 16; // 1 AU * 16 bits
    let mut headers = Vec::with_capacity(4);
    headers.push((headers_length >> 8) as u8);
    headers.push(headers_length as u8);
    // AU header: 13 bits size + 3 bits index
    headers.push((frame_len >> 5) as u8);
    headers.push(((frame_len & 0x1F) << 3) as u8);
    headers
}

pub fn current_nt_timestamp() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let frac = duration.subsec_nanos() as u64;
    ((secs + 2_208_988_800) << 32) | ((frac << 32) / 1_000_000_000)
}

#[derive(Debug)]
pub struct RtcpSenderReport {
    pub ntp_timestamp: u64,
    pub rtp_timestamp: u32,
    pub packet_count: u32,
    pub octet_count: u32,
}

impl RtcpSenderReport {
    pub fn new() -> Self {
        Self {
            ntp_timestamp: current_nt_timestamp(),
            rtp_timestamp: 0,
            packet_count: 0,
            octet_count: 0,
        }
    }

    pub fn update(&mut self, rtp_ts: u32, packet_len: u32) {
        self.ntp_timestamp = current_nt_timestamp();
        self.rtp_timestamp = rtp_ts;
        self.packet_count += 1;
        self.octet_count += packet_len;
    }

    pub fn serialize(&self, ssrc: u32) -> Vec<u8> {
        let mut packet = Vec::with_capacity(28);

        // V=2, P=0, RC=0, PT=200 (SR)
        packet.push(0x80);
        packet.push(200);

        // Length = 6 (24 bytes / 4 - 1)
        let length: u16 = 6;
        packet.push((length >> 8) as u8);
        packet.push(length as u8);

        // SSRC
        packet.extend_from_slice(&ssrc.to_be_bytes());

        // NTP timestamp
        packet.extend_from_slice(&self.ntp_timestamp.to_be_bytes());

        // RTP timestamp
        packet.extend_from_slice(&self.rtp_timestamp.to_be_bytes());

        // Sender's packet count
        packet.extend_from_slice(&self.packet_count.to_be_bytes());

        // Sender's octet count
        packet.extend_from_slice(&self.octet_count.to_be_bytes());

        packet
    }
}
