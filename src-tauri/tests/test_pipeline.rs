//! Exercises the real video path: raw BGRA in → ffmpeg H.264 → Annex-B parse →
//! access-unit grouping → RTP packetization. This is the chain the live
//! session runs, minus the network send and the screen grab.

use aerosend::airplay::h264::{AccessUnitReader, Nal, NalType};
use aerosend::airplay::rtp::RtpPacketizer;
use aerosend::capture::encoder::{probe_encoders, EncoderType, VideoEncoder};
use std::io::Read;
use std::process::Command;
use std::sync::mpsc;
use ffmpeg_sidecar::ffprobe::ffprobe_path;

/// The clock rate H.264 RTP uses, so timestamp spacing can be checked.
const VIDEO_CLOCK: u32 = 90_000;

const W: usize = 320;
const H: usize = 240;
const FPS: u32 = 30;

/// A frame whose content changes every iteration, so the encoder emits a
/// non-trivial bitstream rather than pure skip blocks.
fn synthetic_frame(w: usize, h: usize, i: usize) -> Vec<u8> {
    let mut buf = vec![0u8; w * h * 4];
    for y in 0..h {
        let row = &mut buf[y * w * 4..(y + 1) * w * 4];
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            px[0] = ((x + i) % 256) as u8;
            px[1] = ((y * 2 + i * 3) % 256) as u8;
            px[2] = ((x * y + i) % 256) as u8;
            px[3] = 255;
        }
    }
    buf
}

struct Au {
    nalus: Vec<Nal>,
}

/// Feeds `frames` frames of `in_w`x`in_h` BGRA through the encoder at the
/// given output geometry and returns the access units it produced, in order.
fn encode_frames(
    kind: EncoderType,
    in_w: usize,
    in_h: usize,
    out_w: u32,
    out_h: u32,
    frames: usize,
) -> Vec<Au> {
    let mut encoder = VideoEncoder::new(in_w as u32, in_h as u32, out_w, out_h, FPS, 2_000_000, kind);
    encoder.start().expect("ffmpeg should start");
    let mut stdout = encoder.take_stdout().expect("encoder stdout");

    let (tx, rx) = mpsc::channel::<Vec<Nal>>();

    let reader = {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = AccessUnitReader::new();
            let mut buf = [0u8; 32 * 1024];
            // Read to EOF: closing stdin makes ffmpeg flush and exit, so every
            // encoded picture is in the pipe before the stream ends. Stopping
            // on a flag instead would discard output still in flight.
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                for au in reader.push(&buf[..n]) {
                    if tx.send(au).is_err() {
                        return;
                    }
                }
            }
            for au in reader.finish() {
                let _ = tx.send(au);
            }
        })
    };

    for i in 0..frames {
        encoder
            .send_frame(&synthetic_frame(in_w, in_h, i))
            .expect("encoder stdin should accept frames");
    }
    encoder.stop().expect("encoder stops cleanly");
    drop(tx);
    reader.join().expect("reader thread");

    rx.iter().map(|nalus| Au { nalus }).collect()
}

fn encode_to_access_units(frames: usize) -> Vec<Au> {
    encode_frames(EncoderType::X264, W, H, W as u32, H as u32, frames)
}

#[test]
fn encoder_output_parses_into_real_access_units() {
    const FRAMES: usize = 30;
    let aus = encode_to_access_units(FRAMES);
    assert_eq!(
        aus.len(),
        FRAMES,
        "one fed frame must yield exactly one access unit"
    );

    let first = &aus[0];
    let types: Vec<NalType> = first.nalus.iter().map(|n| n.nal_type()).collect();
    assert!(
        types.contains(&NalType::Sps) && types.contains(&NalType::Pps),
        "first access unit must carry the parameter sets, got {types:?}"
    );
    assert!(
        first.nalus.iter().any(|n| n.is_idr()),
        "first access unit must start with an IDR"
    );

    // Only the leading AU carries parameter sets; later ones are slices alone.
    for au in &aus[1..] {
        let slices: Vec<&Nal> = au.nalus.iter().filter(|n| n.nal_type().is_slice()).collect();
        assert!(!slices.is_empty(), "an access unit must carry a slice");
        assert_eq!(
            slices.iter().filter(|n| n.starts_new_picture()).count(),
            1,
            "one access unit is one picture, however many slices it spans"
        );
        assert!(
            !au.nalus.iter().any(|n| n.nal_type() == NalType::Sps),
            "parameter sets must not repeat mid-stream"
        );
    }

    let bytes: usize = aus
        .iter()
        .flat_map(|a| a.nalus.iter())
        .map(|n| n.data.len())
        .sum();
    println!(
        "{FRAMES} frames -> {} access units, {bytes} encoded bytes ({:.1} kbps)",
        aus.len(),
        bytes as f64 * 8.0 * FPS as f64 / FRAMES as f64 / 1000.0
    );
    assert!(bytes > 10_000, "encoded stream is implausibly small");
}

#[test]
fn packetized_access_units_reassemble_to_the_original_nals() {
    let aus = encode_to_access_units(12);
    assert_eq!(aus.len(), 12, "one access unit per fed frame");

    let mut rtp = RtpPacketizer::new(96, VIDEO_CLOCK);
    let mut sequence: u16 = rtp.sequence_number();
    let mut total_packets = 0usize;
    let mut oversized = 0usize;

    for (i, au) in aus.iter().enumerate() {
        let timestamp = rtp.clock_for_ms(i as u64 * 1000 / FPS as u64);
        let packets = rtp.packetize_h264(&au.nalus, timestamp);
        assert!(!packets.is_empty());
        total_packets += packets.len();

        for pkt in &packets {
            if pkt.len() > 12 + 1400 {
                oversized += 1;
            }
            assert_eq!(pkt[0], 0x80, "RTP version/flags");
            assert_eq!(u16::from_be_bytes([pkt[2], pkt[3]]), sequence);
            sequence = sequence.wrapping_add(1);
            assert_eq!(
                u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]),
                timestamp,
                "one access unit shares one RTP timestamp"
            );
        }

        // The marker bit appears exactly once, on the last packet of the AU.
        let marked: Vec<usize> = packets
            .iter()
            .enumerate()
            .filter(|(_, p)| p[1] & 0x80 != 0)
            .map(|(j, _)| j)
            .collect();
        assert_eq!(marked, vec![packets.len() - 1]);

        // Timestamps must advance one frame period per access unit; the
        // sliced-threads bug used to give three AUs the same picture's ticks.
        let ideal = (i as u32 * VIDEO_CLOCK / FPS) as i64;
        let drift = timestamp as i64 - ideal;
        assert!(
            drift.abs() <= (VIDEO_CLOCK / FPS) as i64 / 10,
            "AU {i} timestamp {timestamp} is {drift} ticks off the frame grid"
        );

        // Reassembly: concatenate single-NAL payloads and FU-A fragments back
        // into NAL units and compare against what the encoder produced.
        let mut rebuilt: Vec<Vec<u8>> = Vec::new();
        for pkt in &packets {
            let payload = &pkt[12..];
            let ty = payload[0] & 0x1F;
            if ty == 28 {
                let start = payload[1] & 0x80 != 0;
                if start || rebuilt.is_empty() {
                    rebuilt.push(vec![(payload[0] & 0x60) | (payload[1] & 0x1F)]);
                }
                rebuilt.last_mut().unwrap().extend_from_slice(&payload[2..]);
            } else {
                rebuilt.push(payload.to_vec());
            }
        }
        let expected: Vec<Vec<u8>> = au.nalus.iter().map(|n| n.data.clone()).collect();
        assert_eq!(rebuilt, expected, "AU {i} did not survive the RTP round trip");
    }

    assert_eq!(oversized, 0, "no packet may exceed the RTP payload cap");
    println!(
        "{} access units -> {total_packets} RTP packets, all reassembling exactly",
        aus.len()
    );
}

/// The Settings page may ask for a smaller stream than the screen produces.
/// The scale filter has to reach the bitstream, because the SDP framesize is
/// what the receiver sizes its surface from.
#[test]
fn scaled_encoding_changes_the_stream_geometry() {
    const IN_W: usize = 640;
    const IN_H: usize = 480;
    let aus = encode_frames(EncoderType::X264, IN_W, IN_H, 320, 240, 10);
    assert_eq!(aus.len(), 10, "scaling must not change the AU-per-frame rule");
    for au in &aus {
        let slices: Vec<&Nal> = au.nalus.iter().filter(|n| n.nal_type().is_slice()).collect();
        assert!(!slices.is_empty(), "a scaled AU must carry a slice");
        assert_eq!(
            slices.iter().filter(|n| n.starts_new_picture()).count(),
            1,
            "scaling must not split a picture across access units"
        );
    }
    assert!(aus[0].nalus.iter().any(|n| n.nal_type() == NalType::Sps));

    let mut annexb = Vec::new();
    for au in &aus {
        for nal in &au.nalus {
            annexb.extend([0u8, 0, 0, 1]);
            annexb.extend(&nal.data);
        }
    }
    let path = std::env::temp_dir().join("aerosend-scaled-stream.264");
    std::fs::write(&path, &annexb).expect("temp stream write");

    let probe = Command::new(ffprobe_path())
        .args(["-v", "error"])
        .args(["-select_streams", "v:0"])
        .args(["-show_entries", "stream=width,height,pix_fmt"])
        .args(["-of", "csv=p=0"])
        .arg(&path)
        .output()
        .expect("ffprobe should run");
    let _ = std::fs::remove_file(&path);
    let text = String::from_utf8_lossy(&probe.stdout);
    println!("ffprobe: {}{}", text.trim(), String::from_utf8_lossy(&probe.stderr).trim());
    assert!(
        text.contains("320,240,yuv420p"),
        "scaled stream should be 320x240 yuv420p, ffprobe said {text:?}"
    );
}

/// "Auto" is a promise about this machine, not a fixed codec name: whatever it
/// picks has to be an encoder the bundled ffmpeg actually offers, and a build
/// without hardware encoding still has to produce a stream.
#[test]
fn auto_resolves_to_an_encoder_ffmpeg_provides() {
    let probe = probe_encoders().expect("bundled ffmpeg should answer -encoders");
    let codec = EncoderType::Auto.as_str();
    let offered = match codec {
        "h264_nvenc" => probe.nvenc,
        "h264_qsv" => probe.qsv,
        "libx264" => probe.x264,
        other => panic!("Auto resolved to an unexpected codec: {other}"),
    };
    println!(
        "ffmpeg {} (x264={} nvenc={} qsv={}) -> Auto picks {codec}",
        probe.ffmpeg, probe.x264, probe.nvenc, probe.qsv
    );
    assert!(
        offered,
        "Auto chose {codec}, which {} does not provide",
        probe.ffmpeg
    );

    let aus = encode_frames(EncoderType::Auto, W, H, W as u32, H as u32, 4);
    assert_eq!(aus.len(), 4, "Auto must still deliver one AU per frame");
    assert!(aus[0].nalus.iter().any(|n| n.nal_type() == NalType::Sps));
}
