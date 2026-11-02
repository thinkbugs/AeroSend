//! The AirPlay mirroring channel: 128-byte headers and one AES-128-CTR
//! keystream over a single TCP connection.
//!
//! This is not RTP. The receiver names a TCP port in its reply to the stream
//! 110 `SETUP`, and every frame after that is a header followed by a payload:
//!
//! ```text
//! 128-byte header | payload
//! ```
//!
//! The header is little-endian, unlike most of AirPlay:
//!
//! | offset | type | meaning |
//! |---|---|---|
//! | 0 | u32 | payload length |
//! | 4 | u16 | payload type: 0 video, 1 codec config |
//! | 6 | u16 | option |
//! | 8 | u64 | NTP timestamp (video only) |
//! | 40, 44 | f32 | source width, height (codec config only) |
//! | 56, 60 | f32 | output width, height (codec config only) |
//!
//! Video payloads are AVCC — 4-byte big-endian lengths followed by NAL units —
//! encrypted with the keystream from [`stream_keys`]. The keystream runs across
//! frame boundaries, so one [`MirrorWriter`] owns one cipher for the life of the
//! connection and a frame cannot be encrypted on its own.
//!
//! The receiver prepends the SPS and PPS from the codec config to every key
//! frame, so each IDR is independently decodable; that is why the codec config
//! has to be the first thing on the socket.

use anyhow::{Context, Result};
use sha2::{Digest, Sha512};
use std::io::Write;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::airplay::aes::AesCtr;
use crate::airplay::h264::Nal;

pub const PAYLOAD_VIDEO: u16 = 0;
pub const PAYLOAD_CODEC: u16 = 1;
const HEADER_LENGTH: usize = 128;

/// Seconds between the NTP epoch (1900) and the Unix epoch (1970).
const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;

/// The mirroring stream cipher, derived the way the receiver derives it:
///
/// ```text
/// eaes = SHA-512(aesKey ‖ shared)
/// key  = SHA-512("AirPlayStreamKey" ‖ connectionID ‖ eaes[0..16])[0..16]
/// iv   = SHA-512("AirPlayStreamIV"  ‖ connectionID ‖ eaes[0..16])[0..16]
/// ```
///
/// `connectionID` is the decimal `streamConnectionID` the sender put in its own
/// SETUP plist, not a binary blob.
pub fn stream_keys(
    aes_key: &[u8],
    ecdh_shared: &[u8],
    connection_id: &str,
) -> Result<([u8; 16], [u8; 16])> {
    if aes_key.len() != 16 {
        anyhow::bail!("mirror aes key is {} bytes, want 16", aes_key.len());
    }
    if ecdh_shared.len() != 32 {
        anyhow::bail!(
            "mirror shared secret is {} bytes, want 32",
            ecdh_shared.len()
        );
    }
    let mut hasher = Sha512::new();
    hasher.update(aes_key);
    hasher.update(ecdh_shared);
    let eaes = hasher.finalize();
    Ok((
        labelled_key("AirPlayStreamKey", connection_id, &eaes),
        labelled_key("AirPlayStreamIV", connection_id, &eaes),
    ))
}

fn labelled_key(label: &str, connection_id: &str, eaes: &[u8]) -> [u8; 16] {
    let mut hasher = Sha512::new();
    hasher.update(label.as_bytes());
    hasher.update(connection_id.as_bytes());
    hasher.update(&eaes[..16]);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// The sender's wall clock as an NTP timestamp: the high half counts seconds
/// since 1900, the low half is the binary fraction of the current second.
///
/// A receiver that measures latency compares this against its own clock, so the
/// field has to be a wall clock rather than a per-session counter.
pub fn ntp_now() -> u64 {
    let since_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = since_unix.as_secs() + NTP_EPOCH_OFFSET;
    let micros = since_unix.subsec_micros() as u64;
    (seconds << 32) | ((micros << 32) / 1_000_000)
}

/// One frame header, ready to write.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameHeader {
    pub size: u32,
    pub payload_type: u16,
    pub option: u16,
    pub ntp: u64,
    /// Source geometry, only meaningful on a codec config.
    pub source: (u32, u32),
    /// Output geometry the sender will actually deliver.
    pub output: (u32, u32),
}

impl FrameHeader {
    pub fn to_bytes(&self) -> [u8; HEADER_LENGTH] {
        let mut bytes = [0u8; HEADER_LENGTH];
        bytes[0..4].copy_from_slice(&self.size.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.payload_type.to_le_bytes());
        bytes[6..8].copy_from_slice(&self.option.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.ntp.to_le_bytes());
        // The geometry fields are floats even though they count pixels; the
        // reader turns them back into integers and treats a non-positive or
        // non-finite value as absent.
        for (at, value) in [
            (40, self.source.0),
            (44, self.source.1),
            (56, self.output.0),
            (60, self.output.1),
        ] {
            bytes[at..at + 4].copy_from_slice(&(value as f32).to_le_bytes());
        }
        bytes
    }

    /// Parses a header the same way the receiver does, so a round trip proves
    /// the writer put each field at the offset the reader asks for.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LENGTH {
            anyhow::bail!("mirror header is {} bytes", bytes.len());
        }
        let real = |at: usize| {
            let value = f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
            if value.is_finite() && value > 0.0 {
                value as u32
            } else {
                0
            }
        };
        Ok(Self {
            size: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            payload_type: u16::from_le_bytes(bytes[4..6].try_into().unwrap()) & 0xff,
            option: u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
            ntp: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            source: (real(40), real(44)),
            output: (real(56), real(60)),
        })
    }
}

/// The codec-config payload: an AVCC `decoderConfig` carrying one SPS and one
/// PPS. Receivers read the parameter set lengths from this and rebuild Annex-B.
pub fn codec_config(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(11 + sps.len() + pps.len());
    // [configuration version, profile, compatibility, level, length size, pps
    // count] — the profile and level are read out of the SPS header so a
    // receiver can choose a decoder before it has seen a single slice.
    let header = [
        1u8,
        sps.get(1).copied().unwrap_or(0x42),
        sps.get(2).copied().unwrap_or(0x00),
        sps.get(3).copied().unwrap_or(0x1e),
        0xe0 | 3,
        0xe1,
    ];
    out.extend_from_slice(&header);
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1);
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    out
}

/// Access units in AVCC: a 4-byte big-endian length in front of every NAL unit,
/// which is what the encoder's Annex-B start codes become on the wire.
pub fn avcc(nalus: &[Nal]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nalus {
        out.extend_from_slice(&(nal.data.len() as u32).to_be_bytes());
        out.extend_from_slice(&nal.data);
    }
    out
}

/// How much video the connection may hold back because the receiver has not
/// taken it yet.
///
/// This is the sender's whole tolerance for a receiver that stops reading. The
/// alternative — hand the bytes to a blocking socket and wait — is what used to
/// happen, and a stalled receiver turned it into a frozen picture followed by
/// the local stack aborting the connection (`os error 10053`). Two megabytes is
/// a few seconds of video at the default bitrate: enough to ride out a
/// scheduling hiccup, old enough that a picture queued behind it is no longer
/// the screen the sender is looking at.
pub const MAX_QUEUED_BYTES: usize = 2 * 1024 * 1024;

/// Writes encoded pictures to one mirroring connection.
///
/// The socket is never allowed to block this writer. Bytes go into a queue and
/// [`MirrorWriter::pump`] hands the socket as much as it will take; when the
/// queue is full, [`MirrorWriter::write_video`] refuses the picture instead of
/// waiting for the receiver.
pub struct MirrorWriter {
    stream: TcpStream,
    cipher: AesCtr,
    /// Queued frame bytes, oldest first, with `written` marking what the socket
    /// has already taken. Keeping whole frames in order is what lets a refused
    /// picture be dropped without breaking the framing or the keystream.
    out: Vec<u8>,
    written: usize,
    /// When the socket last took a byte, or when the queue last ran dry: the
    /// caller's stall clock.
    progressed: Instant,
    pub codec_configs: u64,
    pub frames: u64,
    pub bytes: u64,
}

impl MirrorWriter {
    pub fn connect(addr: &str, aes_key: &[u8], ecdh_shared: &[u8], connection_id: &str) -> Result<Self> {
        let stream =
            TcpStream::connect(addr).with_context(|| format!("mirror connect to {addr}"))?;
        stream.set_nodelay(true).ok();
        // A nonblocking socket is what lets `pump` report a full send buffer
        // instead of parking this thread inside the kernel.
        stream.set_nonblocking(true).ok();
        let (key, iv) = stream_keys(aes_key, ecdh_shared, connection_id)?;
        Ok(Self {
            stream,
            cipher: AesCtr::new(&key, &iv)?,
            out: Vec::with_capacity(256 * 1024),
            written: 0,
            progressed: Instant::now(),
            codec_configs: 0,
            frames: 0,
            bytes: 0,
        })
    }

    /// Bytes still waiting for the receiver.
    pub fn queued(&self) -> usize {
        self.out.len() - self.written
    }

    /// How long the socket has gone without accepting a byte, or zero when
    /// everything queued has already left.
    pub fn stalled_for(&self) -> Duration {
        if self.queued() == 0 {
            Duration::ZERO
        } else {
            self.progressed.elapsed()
        }
    }

    /// Hands the socket everything it will take right now. Never blocks.
    pub fn pump(&mut self) -> Result<()> {
        while self.written < self.out.len() {
            match self.stream.write(&self.out[self.written..]) {
                Ok(0) => {
                    anyhow::bail!("the mirroring connection accepted nothing and is gone")
                }
                Ok(written) => {
                    self.written += written;
                    self.progressed = Instant::now();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("the mirroring socket refused data"),
            }
        }
        if self.written == self.out.len() {
            // Drained: the stall clock restarts, and the next queued frame is
            // the start of a new run rather than the tail of the old one.
            self.out.clear();
            self.written = 0;
            self.progressed = Instant::now();
        }
        Ok(())
    }

    fn queue(&mut self, header: &FrameHeader, payload: &[u8]) {
        if self.written == self.out.len() {
            self.out.clear();
            self.written = 0;
            self.progressed = Instant::now();
        }
        self.out.extend_from_slice(&header.to_bytes());
        self.out.extend_from_slice(payload);
    }

    /// The parameter sets, sent once at the start and again whenever the encoder
    /// changes them. Geometry travels with the config, not with the frames.
    ///
    /// A codec config is never dropped: every key frame the receiver rebuilds
    /// needs it, so it goes into the queue ahead of the pictures behind it.
    pub fn write_codec_config(
        &mut self,
        sps: &[u8],
        pps: &[u8],
        source: (u32, u32),
        output: (u32, u32),
    ) -> Result<()> {
        let payload = codec_config(sps, pps);
        let header = FrameHeader {
            size: payload.len() as u32,
            payload_type: PAYLOAD_CODEC,
            option: 0,
            ntp: 0,
            source,
            output,
        };
        // The codec config is the one payload the receiver reads in the clear.
        self.queue(&header, &payload);
        self.pump()?;
        self.codec_configs += 1;
        self.bytes += HEADER_LENGTH as u64 + payload.len() as u64;
        Ok(())
    }

    /// One coded picture: AVCC framing, then the stream cipher, then the header.
    /// The cipher state is never reset, so the receiver's keystream lines up
    /// across every frame of the connection.
    ///
    /// Returns `false` when the picture is refused because the receiver has not
    /// taken what is already queued. The check comes before the cipher runs for
    /// that reason: the keystream advances with every byte of every frame, so a
    /// frame cannot be encrypted and then dropped.
    pub fn write_video(&mut self, nalus: &[Nal], ntp: u64) -> Result<bool> {
        if self.queued() + HEADER_LENGTH >= MAX_QUEUED_BYTES {
            return Ok(false);
        }
        let mut payload = avcc(nalus);
        self.cipher.apply_keystream(&mut payload);
        let header = FrameHeader {
            size: payload.len() as u32,
            payload_type: PAYLOAD_VIDEO,
            option: 0,
            ntp,
            source: (0, 0),
            output: (0, 0),
        };
        self.queue(&header, &payload);
        self.pump()?;
        self.frames += 1;
        self.bytes += HEADER_LENGTH as u64 + payload.len() as u64;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    #[test]
    fn a_header_survives_the_writer_reader_round_trip() {
        let header = FrameHeader {
            size: 4096,
            payload_type: PAYLOAD_VIDEO,
            option: 0,
            ntp: (77 << 32) | 0x4000_0000,
            source: (0, 0),
            output: (0, 0),
        };
        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), HEADER_LENGTH);
        assert_eq!(FrameHeader::parse(&bytes).unwrap(), header);
    }

    #[test]
    fn the_codec_config_geometry_lands_where_the_reader_looks() {
        let header = FrameHeader {
            size: 16,
            payload_type: PAYLOAD_CODEC,
            option: 0,
            ntp: 0,
            source: (1920, 1080),
            output: (1280, 720),
        };
        let parsed = FrameHeader::parse(&header.to_bytes()).unwrap();
        assert_eq!(parsed.source, (1920, 1080));
        assert_eq!(parsed.output, (1280, 720));
    }

    #[test]
    fn the_ntp_field_carries_the_wall_clock() {
        let ntp = ntp_now();
        let seconds = ntp >> 32;
        // Well past 2020 and well before 2100, and never the Unix epoch: a
        // receiver that compares this with its own clock would see a 70-year
        // error if the offset were missing.
        assert!(seconds > 3_800_000_000, "ntp seconds {seconds}");
        let fraction = ntp & 0xffff_ffff;
        assert!(fraction < (1u64 << 32));
        let micros = (fraction as u128 * 1_000_000) >> 32;
        assert!(micros < 1_000_000, "subsecond {micros} out of range");
    }

    /// The receiver reads the SPS length at bytes 6-7 and the PPS length three
    /// bytes after the SPS; a codec config it cannot parse means no key frame is
    /// ever decodable, so the layout is checked against those offsets directly.
    #[test]
    fn the_codec_config_uses_the_avcc_layout_the_receiver_reads() {
        let sps = vec![0x67u8, 0x42, 0x00, 0x1e, 0xab, 0xcd];
        let pps = vec![0x68u8, 0xce, 0x38, 0x80];
        let data = codec_config(&sps, &pps);

        assert_eq!(data[0], 1);
        assert_eq!(data[4] & 0x1f, 3, "length size minus one");
        assert_eq!(data[5] & 0x07, 1, "one pps");
        assert_eq!(u16::from_be_bytes([data[6], data[7]]) as usize, sps.len());
        let sps_end = 8 + sps.len();
        assert_eq!(&data[8..sps_end], sps.as_slice());
        assert_eq!(data[sps_end], 1);
        let pps_length =
            u16::from_be_bytes([data[sps_end + 1], data[sps_end + 2]]) as usize;
        assert_eq!(pps_length, pps.len());
        assert_eq!(&data[sps_end + 3..sps_end + 3 + pps_length], pps.as_slice());
        // The profile and level come from the SPS so a receiver can pick a
        // decoder before it has seen a slice.
        assert_eq!(data[1], 0x42);
        assert_eq!(data[3], 0x1e);
    }

    #[test]
    fn avcc_prefixes_every_nal_with_a_big_endian_length() {
        let nalus = vec![
            Nal {
                data: vec![0x65, 1, 2, 3],
            },
            Nal {
                data: vec![0x41, 4],
            },
        ];
        assert_eq!(
            avcc(&nalus),
            vec![0, 0, 0, 4, 0x65, 1, 2, 3, 0, 0, 0, 2, 0x41, 4]
        );
    }

    /// The keystream is defined by the label, the decimal connection id and the
    /// FairPlay-derived key, so a fixed vector pins all three.
    #[test]
    fn stream_keys_match_a_frozen_vector() {
        let aes_key = [0x42u8; 16];
        let shared = [0x21u8; 32];
        let (key, iv) = stream_keys(&aes_key, &shared, "123456789012").unwrap();
        let mut check = Sha512::new();
        check.update(&aes_key);
        check.update(&shared);
        let eaes = check.finalize();
        let mut expected = Sha512::new();
        expected.update(b"AirPlayStreamKey");
        expected.update(b"123456789012");
        expected.update(&eaes[..16]);
        assert_eq!(key.to_vec(), expected.finalize()[..16].to_vec());
        assert_ne!(key, iv);
    }

    #[test]
    fn stream_keys_reject_short_key_material() {
        assert!(stream_keys(&[0u8; 15], &[0u8; 32], "1").is_err());
        assert!(stream_keys(&[0u8; 16], &[0u8; 31], "1").is_err());
    }

    /// Encryption is a single stream across frames: two frames encrypted in
    /// sequence must not be the same as each frame encrypted from a fresh
    /// cipher, which is exactly the mistake that makes a receiver's first frame
    /// decode and the rest show as noise.
    #[test]
    fn the_cipher_runs_on_across_frames() {
        let key = [7u8; 16];
        let iv = [9u8; 16];
        let frame_a = vec![0xaau8; 1000];
        let frame_b = vec![0xbbu8; 1000];

        let mut continuous = AesCtr::new(&key, &iv).unwrap();
        let first = continuous.process(&frame_a);
        let second = continuous.process(&frame_b);

        let mut restarted = AesCtr::new(&key, &iv).unwrap();
        assert_eq!(restarted.process(&frame_a), first);
        let mut fresh = AesCtr::new(&key, &iv).unwrap();
        let solo = fresh.process(&frame_b);
        assert_ne!(solo, second, "the counter must not restart between frames");
    }

    /// The queue is emptied by handing it to a peer that reads; the writer only
    /// moves bytes when the thread that owns it calls `pump`.
    fn drain(writer: &mut MirrorWriter) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while writer.queued() > 0 {
            writer.pump().unwrap();
            if writer.queued() > 0 {
                assert!(Instant::now() < deadline, "the socket never drained");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    /// A receiver that stops reading must cost the session as few frames as
    /// possible and no framing or keystream damage. Those are the same
    /// assertion: the keystream advances with every encrypted byte, so a frame
    /// that is encrypted and then dropped would decrypt the rest of the session
    /// into noise.
    ///
    /// The backlog is fabricated rather than accumulated through the socket,
    /// because how many bytes a loopback connection takes before it reports
    /// `WouldBlock` is the OS's choice, not the writer's.
    #[test]
    fn a_refused_frame_leaves_the_keystream_aligned() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let aes_key = [3u8; 16];
        let shared = [0u8; 32];
        let mut writer =
            MirrorWriter::connect(&addr.to_string(), &aes_key, &shared, "42").unwrap();
        let peer = listener.accept().unwrap().0;
        // Reading runs on its own thread for the whole session, so the writer
        // can always hand bytes over.
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut peer = peer;
            let mut all = Vec::new();
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                match peer.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => all.extend_from_slice(&chunk[..read]),
                }
            }
            let _ = tx.send(all);
        });

        let sps = vec![0x67u8, 0x42, 0x00, 0x1e];
        let pps = vec![0x68u8, 0xce, 0x38, 0x80];
        writer
            .write_codec_config(&sps, &pps, (1920, 1080), (1920, 1080))
            .unwrap();

        // The pattern comes from the frame index, so a reordered, duplicated or
        // skipped frame cannot pass the read-back check.
        fn picture(index: u8) -> Nal {
            Nal {
                data: (0..64 * 1024).map(|at| (at as u8).wrapping_add(index)).collect(),
            }
        }
        let mut accepted: Vec<Vec<u8>> = Vec::new();
        let first = picture(0);
        accepted.push(avcc(&[first.clone()]));
        assert!(writer.write_video(&[first.clone()], ntp_now()).unwrap());
        drain(&mut writer);

        let frames_before = writer.frames;
        let bytes_before = writer.bytes;
        // The receiver stops reading: the queue is at the watermark.
        writer.out.resize(MAX_QUEUED_BYTES, 0);
        let second = picture(1);
        accepted.push(avcc(&[second.clone()]));
        assert!(
            !writer.write_video(&[second.clone()], ntp_now()).unwrap(),
            "a full queue still accepted a frame"
        );
        assert_eq!(writer.frames, frames_before, "a refused frame was counted");
        assert_eq!(writer.bytes, bytes_before, "a refused frame was billed");
        assert_eq!(
            writer.out.len(),
            MAX_QUEUED_BYTES,
            "a refused frame was queued"
        );

        // The receiver comes back, and the frame that was refused is sent for
        // real: it must land exactly where a cipher that only ever saw the
        // accepted frames expects it.
        writer.out.clear();
        writer.written = 0;
        assert!(writer.write_video(&[second], ntp_now()).unwrap());
        drain(&mut writer);

        writer.stream.shutdown(std::net::Shutdown::Write).unwrap();
        let wire = rx.recv().unwrap();

        let (mirror_key, mirror_iv) = stream_keys(&aes_key, &shared, "42").unwrap();
        let mut decipher = AesCtr::new(&mirror_key, &mirror_iv).unwrap();
        let codec = codec_config(&sps, &pps);
        let mut at = 0usize;
        let mut frames_read = 0usize;
        while at < wire.len() {
            let header = FrameHeader::parse(&wire[at..]).unwrap();
            at += HEADER_LENGTH;
            assert!(
                at + header.size as usize <= wire.len(),
                "a header claims more bytes than arrived"
            );
            let payload = &wire[at..at + header.size as usize];
            at += header.size as usize;
            if header.payload_type == PAYLOAD_CODEC {
                assert_eq!(payload, codec.as_slice());
                continue;
            }
            assert_eq!(
                payload,
                decipher.process(&accepted[frames_read]).as_slice(),
                "frame {frames_read} arrived out of order or with a broken keystream"
            );
            frames_read += 1;
        }
        assert_eq!(at, wire.len(), "the writer left the framing uneven");
        assert_eq!(frames_read, accepted.len());
    }
}
