//! The mirrored-audio path short of the socket: synthetic PCM through the same
//! ffmpeg AAC-LC encoder the mirror session uses, the same ADTS demuxer, and the
//! same CBC encryptor for each RTP payload.
//!
//! What this proves: the encoder really produces AAC-LC ADTS frames, the demuxer
//! cuts them out by their own lengths, timestamps advance by one access unit,
//! and the payload on the wire is ciphertext whose trailing partial block stays
//! in the clear (RAOP copies it rather than padding).
//!
//! What it cannot prove: that a receiver decodes those frames into sound. The
//! HORIZON receiver drains stream 96 without a decoder, and no other receiver is
//! on this machine. `docs/TEST-REPORT-phase8.md` records that limit.

use aerosend::airplay::aes::AesCbc;
use aerosend::airplay::rtp::RtpPacketizer;
use aerosend::capture::adts::AdtsParser;
use aerosend::capture::audio::AudioFormat;
use aerosend::capture::audio_encoder::AudioEncoder;
use std::io::Read;

const IN_RATE: u32 = 48_000;
const SECONDS: usize = 2;
/// One AAC-LC access unit at 44.1 kHz.
const FRAMES_PER_PACKET: u32 = 1024;
const KEY: [u8; 16] = [0x2b; 16];
const IV: [u8; 16] = [0x7e; 16];

fn tone_pcm() -> Vec<u8> {
    let frames = IN_RATE as usize * SECONDS;
    let mut pcm = Vec::with_capacity(frames * 8);
    for i in 0..frames {
        let value = (i as f32 * 440.0 * std::f32::consts::TAU / IN_RATE as f32).sin() * 0.25;
        let sample = value.to_le_bytes();
        pcm.extend_from_slice(&sample);
        pcm.extend_from_slice(&sample);
    }
    pcm
}

#[test]
fn a_tone_becomes_aac_packets_ready_for_the_audio_stream() {
    let format = AudioFormat {
        sample_rate: IN_RATE,
        channels: 2,
        bits_per_sample: 32,
        is_float: true,
        block_align: 8,
    };
    let mut encoder = AudioEncoder::aac_lc(format, 128_000);
    encoder.start().expect("ffmpeg should open the AAC-LC encoder");
    let mut stdout = encoder.take_stdout().expect("encoder stdout");

    let reader = std::thread::spawn(move || {
        let mut parser = AdtsParser::default();
        let mut frames = Vec::new();
        let mut chunk = [0u8; 8 * 1024];
        while let Ok(n) = stdout.read(&mut chunk) {
            if n == 0 {
                break;
            }
            parser.push(&chunk[..n]);
            while let Some(frame) = parser.next_frame() {
                frames.push(frame.data);
            }
        }
        frames
    });

    encoder
        .send_pcm(&tone_pcm())
        .expect("the encoder should take the PCM");
    encoder.stop().expect("encoder stop");
    let frames = reader.join().expect("reader");

    // Two seconds at 1024 samples per frame after the resample to 44.1 kHz.
    // ffmpeg adds its encoder delay, so the count is a range.
    let expected = (44_100 * SECONDS / FRAMES_PER_PACKET as usize) as i64;
    let count = frames.len() as i64;
    assert!(
        (count - expected).abs() <= 3,
        "{count} frames for {SECONDS}s, expected about {expected}"
    );
    assert!(frames.iter().all(|f| !f.is_empty()), "an empty access unit");

    let cbc = AesCbc::new(&KEY, &IV).unwrap();
    let mut rtp = RtpPacketizer::new(96, 44_100);
    let mut timestamp = 0u32;
    let mut previous_sequence: Option<u16> = None;
    for frame in &frames {
        let mut packet = rtp.packetize_audio(frame, timestamp, false);
        assert_eq!(packet[0], 0x80);
        assert_eq!(packet[1], 0x60, "payload type 96 with the marker clear");
        assert_eq!(
            u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
            timestamp
        );
        if let Some(previous) = previous_sequence {
            let sequence = u16::from_be_bytes([packet[2], packet[3]]);
            assert_eq!(sequence, previous.wrapping_add(1));
        }
        previous_sequence = Some(u16::from_be_bytes([packet[2], packet[3]]));

        cbc.encrypt_packet(&mut packet[12..]);
        let wire = &packet[12..];
        assert_eq!(wire.len(), frame.len());
        let blocks = frame.len() / 16 * 16;
        if blocks > 0 {
            assert_ne!(&wire[..16], &frame[..16], "the payload has to leave encrypted");
        }
        // A trailing partial block is copied rather than padded; a frame
        // shorter than one block therefore travels in the clear, as RAOP does.
        assert_eq!(
            &wire[blocks..],
            &frame[blocks..],
            "RAOP leaves what does not fill a block in the clear"
        );

        timestamp += FRAMES_PER_PACKET;
    }
}
