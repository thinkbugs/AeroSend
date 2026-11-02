//! Mirrored audio for the AirPlay 2 mirroring session (SETUP stream 96).
//!
//! WASAPI loopback goes through ffmpeg into ADTS-framed AAC-LC, the ADTS headers
//! are stripped (the receiver gets raw access units, as with RAOP), and every
//! frame becomes one RTP packet on the UDP port the receiver named. The payload
//! is AES-128-CBC with the IV restored per packet; the 12-byte RTP header stays
//! in the clear. A separate control socket carries the periodic RTP↔NTP sync
//! packet — a receiver in the UxPlay lineage will not dequeue audio until it
//! has one.
//!
//! Apple's own senders put AAC-ELD on this stream. ffmpeg's stock Windows build
//! has no AAC-ELD encoder (that needs libfdk_aac), so this sender negotiates
//! AAC-LC instead: it is the nearest format the bundled encoder can really
//! produce, and receivers that dispatch on the announced compression type take
//! it. See `docs/TEST-REPORT-phase8.md` for what that does and does not prove.

use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::airplay::aes::AesCbc;
use crate::airplay::handshake::MIRROR_AUDIO_SAMPLES_PER_PACKET;
use crate::airplay::rtp::{current_nt_timestamp, RtpPacketizer};
use crate::capture::adts::AdtsParser;
use crate::capture::audio::AudioCapture;
use crate::capture::audio_encoder::AudioEncoder;
use crate::stream::session::{StreamStats, AUDIO_CLOCK, AUDIO_PAYLOAD_TYPE};

/// One AAC-LC access unit. The RTP clock advances by this per packet.
const FRAMES_PER_PACKET: u32 = MIRROR_AUDIO_SAMPLES_PER_PACKET as u32;
/// AAC-LC at a bitrate that stays under 200 kbps once UDP/IP overhead is added.
const AAC_BITRATE: u32 = 128_000;
/// How often the RTP↔NTP mapping is repeated on the control socket.
const SYNC_INTERVAL: Duration = Duration::from_secs(3);

/// Starts the audio pump on its own thread. It ends when `running` clears, the
/// same flag the video pumps watch; a missing audio device or encoder is an
/// error for this thread only, and the caller keeps mirroring video.
pub fn spawn(
    host: String,
    ports: (u16, u16),
    control: UdpSocket,
    key: [u8; 16],
    iv: [u8; 16],
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<StreamStats>>,
) -> JoinHandle<Result<()>> {
    std::thread::Builder::new()
        .name("mirror-audio".to_string())
        .spawn(move || -> Result<()> {
            let (receiver_control, receiver_data) = ports;
            control
                .connect((host.as_str(), receiver_control))
                .context("mirror audio control socket")?;
            let data = UdpSocket::bind("0.0.0.0:0").context("mirror audio data socket")?;
            data.connect((host.as_str(), receiver_data))
                .context("mirror audio data socket")?;

            // COM interfaces for WASAPI are not `Send`, so capture, encoder and
            // pump all live on this thread.
            let capture = AudioCapture::new().context("system audio capture is unavailable")?;
            let format = capture.format().clone();
            let mut encoder = AudioEncoder::aac_lc(format.clone(), AAC_BITRATE);
            encoder.start().context("the AAC encoder would not start")?;
            let mut stdout = encoder
                .take_stdout()
                .context("the AAC encoder has no stdout")?;
            capture
                .start()
                .context("system audio capture would not start")?;
            info!(
                "Mirror audio live: {}Hz {}ch -> AAC-LC 44100Hz 2ch on UDP {}",
                format.sample_rate, format.channels, receiver_data
            );

            let (frame_tx, frame_rx) = mpsc::sync_channel::<Vec<u8>>(16);
            let reader_running = running.clone();
            let reader = std::thread::spawn(move || {
                let mut parser = AdtsParser::default();
                let mut chunk = [0u8; 8 * 1024];
                while reader_running.load(Ordering::SeqCst) {
                    match stdout.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            parser.push(&chunk[..n]);
                            while let Some(frame) = parser.next_frame() {
                                if frame_tx.send(frame.data).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            });

            let mut rtp = RtpPacketizer::new(AUDIO_PAYLOAD_TYPE, AUDIO_CLOCK);
            let cbc = AesCbc::new(&key, &iv)?;
            let mut samples: u64 = 0;
            let mut packets: u64 = 0;
            let mut bytes: u64 = 0;
            let mut first_sync = true;
            let mut sync_due = Instant::now();
            let mut poll_gap = Duration::from_millis(5);
            let mut outcome = Ok(());

            while running.load(Ordering::SeqCst) {
                if let Some(chunk) = capture.poll() {
                    if encoder.send_pcm(&chunk.pcm).is_err() {
                        outcome = Err(anyhow!("the AAC encoder stdin closed"));
                        break;
                    }
                    // Poll about half a buffer ahead, the same pacing the RAOP
                    // pump uses, so WASAPI never backs up behind ffmpeg.
                    let frames = chunk.pcm.len() / format.block_align.max(1) as usize;
                    poll_gap = Duration::from_micros(
                        (frames as u64 * 1_000_000 / format.sample_rate.max(1) as u64 / 2)
                            .max(1_000),
                    );
                }

                let frame = match frame_rx.try_recv() {
                    Ok(frame) => frame,
                    Err(mpsc::TryRecvError::Empty) => {
                        std::thread::sleep(poll_gap);
                        continue;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => break,
                };

                let timestamp = samples as u32;
                samples += FRAMES_PER_PACKET as u64;
                let mut packet = rtp.packetize_audio(&frame, timestamp, false);
                // Only whole blocks are encrypted; a trailing partial block
                // stays in the clear, which is how RAOP sends it.
                cbc.encrypt_packet(&mut packet[12..]);
                if let Err(e) = data.send(&packet) {
                    outcome = Err(anyhow!("mirror audio send failed: {e}"));
                    break;
                }
                packets += 1;
                bytes += packet.len() as u64;

                {
                    let mut s = stats.lock().unwrap();
                    s.audio_frames_sent += FRAMES_PER_PACKET as u64;
                    s.rtp_packets_sent += 1;
                    s.audio_bytes_sent = bytes;
                }

                if sync_due.elapsed() >= SYNC_INTERVAL {
                    if let Err(e) =
                        control.send(&sync_packet(first_sync, samples as u32, current_nt_timestamp()))
                    {
                        warn!("Mirror audio sync packet failed: {e}");
                    }
                    first_sync = false;
                    sync_due = Instant::now();
                }
            }

            capture.stop();
            let _ = encoder.stop();
            let _ = reader.join();
            info!(
                "Mirror audio pump stopped: {packets} packets, {samples} sample frames, \
                 {:.2} MB",
                bytes as f64 / 1_000_000.0
            );
            outcome
        })
        .expect("failed to spawn the mirror audio thread")
}

/// The 20-byte RTP↔NTP mapping sent on the audio control socket.
///
/// `packet[1] = 0xd4` marks it as a sync rather than audio, `packet[0]` is 0x90
/// for the first one and 0x80 afterwards, and `packet[16..20]` names a timestamp
/// 0.17 s ahead — the interval Apple uses for AAC streams. Receivers in the
/// UxPlay lineage read only the first two fields.
fn sync_packet(first: bool, rtp_sync: u32, ntp: u64) -> [u8; 20] {
    let mut packet = [0u8; 20];
    packet[0] = if first { 0x90 } else { 0x80 };
    packet[1] = 0xD4;
    packet[2] = 0x00;
    packet[3] = 0x04;
    packet[4..8].copy_from_slice(&rtp_sync.to_be_bytes());
    packet[8..16].copy_from_slice(&ntp.to_be_bytes());
    let next = rtp_sync.wrapping_add(AUDIO_CLOCK / 100 * 17);
    packet[16..20].copy_from_slice(&next.to_be_bytes());
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sync_packet_names_the_mapping_and_marks_the_first_one() {
        let first = sync_packet(true, 44_100, 0x1122_3344_5566_7788);
        assert_eq!(&first[0..2], &[0x90, 0xD4]);
        assert_eq!(&first[2..4], &[0x00, 0x04]);
        assert_eq!(u32::from_be_bytes([first[4], first[5], first[6], first[7]]), 44_100);
        assert_eq!(
            u64::from_be_bytes(first[8..16].try_into().unwrap()),
            0x1122_3344_5566_7788
        );

        let later = sync_packet(false, 44_100, 0);
        assert_eq!(later[0], 0x80);
        // 0.17 s of a 44.1 kHz clock.
        assert_eq!(
            u32::from_be_bytes([later[16], later[17], later[18], later[19]]),
            44_100 + 7_497
        );
    }

    #[test]
    fn a_sync_packet_wraps_with_the_rtp_clock() {
        let packet = sync_packet(false, u32::MAX - 10, 0);
        let next = u32::from_be_bytes([
            packet[16],
            packet[17],
            packet[18],
            packet[19],
        ]);
        assert_eq!(next, (u32::MAX - 10).wrapping_add(7_497));
    }
}
