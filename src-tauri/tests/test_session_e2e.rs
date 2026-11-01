//! End-to-end loopback test: the real session — DXGI capture, ffmpeg encode,
//! RTSP handshake, RTP/RTCP send — driving a fake AirPlay receiver inside this
//! process. Everything below the wire is production code; everything above it
//! is a black-box receiver that only knows the published protocol.
//!
//! Needs a desktop session and an audio render device, because the session
//! captures the actual screen and system audio.

use aerosend::capture::encoder::EncoderType;
use aerosend::stream::session::{AUDIO_PAYLOAD_TYPE, Resolution, VIDEO_CLOCK, VIDEO_PAYLOAD_TYPE};
use aerosend::stream::{StreamSession, StreamSettings};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RUN_FOR: Duration = Duration::from_secs(4);

#[derive(Debug, Default)]
struct ReceiverLog {
    methods: Vec<String>,
    sdp: String,
}

struct Request {
    method: String,
    cseq: u32,
    transport: String,
    body: String,
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let method = line.split_whitespace().next()?.to_string();

    let mut cseq = 0;
    let mut content_length = 0usize;
    let mut transport = String::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        match name.trim().to_ascii_lowercase().as_str() {
            "cseq" => cseq = value.trim().parse().unwrap_or(0),
            "content-length" => content_length = value.trim().parse().unwrap_or(0),
            "transport" => transport = value.trim().to_string(),
            _ => {}
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method,
        cseq,
        transport,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

fn respond(stream: &mut TcpStream, cseq: u32, headers: &[(&str, &str)]) {
    let mut msg = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n");
    for (name, value) in headers {
        msg.push_str(&format!("{name}: {value}\r\n"));
    }
    msg.push_str("\r\n");
    let _ = stream.write_all(msg.as_bytes());
    let _ = stream.flush();
}

/// RTP wants an even port and RTCP the next odd one, and ephemeral binds are
/// not guaranteed to land on such a pair, so probe for a free one.
/// A tone on the default render device, so loopback capture has something to
/// mirror. `ffplay -autoexit` stops on its own; the caller still reaps it.
fn spawn_tone_source(duration_secs: u64) -> std::process::Child {
    std::process::Command::new("ffplay")
        .args([
            "-nodisp",
            "-autoexit",
            "-loglevel",
            "quiet",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={duration_secs}"),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn the ffplay tone used as the loopback source")
}

fn bind_pair() -> (UdpSocket, UdpSocket) {
    let spread = (std::process::id() * 7) % 14_000;
    for attempt in 0..200u32 {
        let base = ((30_000 + spread + attempt * 2) & !1) as u16;
        let local: SocketAddr = format!("127.0.0.1:{base}").parse().unwrap();
        if let Ok(rtp) = UdpSocket::bind(local) {
            let rtcp_addr: SocketAddr = format!("127.0.0.1:{}", base + 1).parse().unwrap();
            if let Ok(rtcp) = UdpSocket::bind(rtcp_addr) {
                return (rtp, rtcp);
            }
        }
    }
    panic!("could not find a free even/odd UDP port pair for the fake receiver");
}

fn collect(
    socket: UdpSocket,
    running: Arc<AtomicBool>,
    out: Arc<Mutex<Vec<Vec<u8>>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        // Non-blocking polling rather than a read timeout: the error kind a
        // timed-out recv reports is OS-specific, and an unrecognised kind would
        // end the collector before a single packet was counted.
        socket
            .set_nonblocking(true)
            .expect("collector socket should be non-blocking");
        // Spinning, not sleeping: the sender pushes a whole keyframe into an 8
        // KiB receive buffer in one tight loop, so any pause here shows up as
        // lost RTP sequence numbers. Four threads for a few seconds is cheap on
        // this machine and the alternative is a test that fails at random.
        let mut buf = [0u8; 2048];
        while running.load(Ordering::SeqCst) {
            match socket.recv(&mut buf) {
                Ok(n) => {
                    let mut packets = out.lock().unwrap();
                    if packets.len() < 50_000 {
                        packets.push(buf[..n].to_vec());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }
    })
}

/// Depacketize RFC 6184 single-NAL and FU-A payloads into NAL units per
/// RTP timestamp, the way a receiver would.
fn group_by_timestamp(packets: &[Vec<u8>]) -> BTreeMap<u32, Vec<Vec<u8>>> {
    let mut units: BTreeMap<u32, Vec<Vec<u8>>> = BTreeMap::new();
    let mut in_flight: BTreeMap<u32, Vec<u8>> = BTreeMap::new();

    for packet in packets {
        if packet.len() < 13 {
            continue;
        }
        let marker = packet[1] & 0x80 != 0;
        let _ = marker;
        let timestamp = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        let payload = &packet[12..];
        let nal_type = payload[0] & 0x1F;

        if nal_type == 28 {
            let fu = payload[1];
            if fu & 0x80 != 0 {
                let header = (payload[0] & 0xE0) | (fu & 0x1F);
                in_flight.insert(timestamp, vec![header]);
            }
            if let Some(nal) = in_flight.get_mut(&timestamp) {
                nal.extend_from_slice(&payload[2..]);
            }
            if fu & 0x40 != 0 {
                if let Some(nal) = in_flight.remove(&timestamp) {
                    units.entry(timestamp).or_default().push(nal);
                }
            }
        } else {
            units.entry(timestamp).or_default().push(payload.to_vec());
        }
    }
    units
}

#[test]
fn session_delivers_a_complete_airplay_stream_over_loopback() {
    // The session reports a degraded sub-path (e.g. no audio device) through
    // tracing, not through its Result, so the log has to be visible here.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")))
        .try_init();

    let listener = TcpListener::bind("127.0.0.1:0").expect("rtsp bind");
    let rtsp_port = listener.local_addr().unwrap().port();
    let (video_rtp, video_rtcp) = bind_pair();
    let (audio_rtp, audio_rtcp) = bind_pair();
    let video_port = video_rtp.local_addr().unwrap().port();
    let audio_port = audio_rtp.local_addr().unwrap().port();

    let log = Arc::new(Mutex::new(ReceiverLog::default()));
    let recording = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(true));

    let video_packets = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let video_reports = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let audio_packets = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let audio_reports = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let collectors = [
        collect(video_rtp, running.clone(), video_packets.clone()),
        collect(video_rtcp, running.clone(), video_reports.clone()),
        collect(audio_rtp, running.clone(), audio_packets.clone()),
        collect(audio_rtcp, running.clone(), audio_reports.clone()),
    ];

    let server_log = log.clone();
    let server_recording = recording.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("rtsp accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("rtsp read timeout");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut setups = 0;

        while let Some(request) = read_request(&mut reader) {
            server_log.lock().unwrap().methods.push(request.method.clone());

            match request.method.as_str() {
                "OPTIONS" => respond(&mut stream, request.cseq, &[("Public", "OPTIONS,ANNOUNCE,SETUP,RECORD,TEARDOWN,GET_PARAMETER")]),
                "ANNOUNCE" => {
                    server_log.lock().unwrap().sdp = request.body.clone();
                    respond(&mut stream, request.cseq, &[]);
                }
                "SETUP" => {
                    assert!(
                        request.transport.starts_with("RTP/AVP/UDP;unicast"),
                        "unexpected transport: {}",
                        request.transport
                    );
                    assert!(
                        request.transport.contains("client_port="),
                        "the sender must name its client ports: {}",
                        request.transport
                    );
                    setups += 1;
                    let port = if setups == 1 { video_port } else { audio_port };
                    respond(
                        &mut stream,
                        request.cseq,
                        &[
                            (
                                "Transport",
                                &format!(
                                    "RTP/AVP/UDP;unicast;destination=127.0.0.1;source=127.0.0.1;server_port={port}-{}",
                                    port + 1
                                ),
                            ),
                            ("Session", "1910829328;timeout=30"),
                        ],
                    );
                }
                "RECORD" => {
                    server_recording.store(true, Ordering::SeqCst);
                    respond(&mut stream, request.cseq, &[("RTP-Info", "seq=1")]);
                }
                "TEARDOWN" => {
                    respond(&mut stream, request.cseq, &[]);
                    break;
                }
                other => respond(&mut stream, request.cseq, &[("X-Unknown", other)]),
            }
        }
    });

    let session = Arc::new(StreamSession::new());
    // WASAPI loopback mirrors what the render device is playing; on an idle
    // machine it returns nothing at all, so the tone is part of the fixture
    // rather than an assumption about what the operator happens to be listening
    // to. Without it the audio path is legitimately empty and this test only
    // passes by chance.
    let tone = spawn_tone_source(RUN_FOR.as_secs() + 8);
    thread::sleep(Duration::from_millis(600));
    let settings = StreamSettings {
        fps: 30,
        bitrate: 2_000_000,
        audio: true,
        resolution: Resolution::Native,
        encoder: EncoderType::X264,
        annotation: false,
    };

    let thread_session = session.clone();
    let session_thread = thread::spawn(move || thread_session.start("127.0.0.1", rtsp_port, &settings));

    let deadline = Instant::now() + Duration::from_secs(25);
    while !recording.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let methods = log.lock().unwrap().methods.clone();
    assert!(
        recording.load(Ordering::SeqCst),
        "handshake never reached RECORD; observed {methods:?}"
    );

    thread::sleep(RUN_FOR);
    session.stop();
    let result = session_thread.join().expect("session thread");
    running.store(false, Ordering::SeqCst);
    for handle in collectors {
        let _ = handle.join();
    }
    let _ = server.join();
    assert!(result.is_ok(), "session ended with an error: {result:?}");

    let log = log.lock().unwrap();
    assert_eq!(
        log.methods,
        vec![
            "OPTIONS",
            "ANNOUNCE",
            "SETUP",
            "SETUP",
            "RECORD",
            "TEARDOWN"
        ],
        "RTSP dialogue"
    );
    println!("SDP announced:\n{}", log.sdp);
    assert!(log.sdp.contains("m=video"), "SDP must advertise video");
    assert!(log.sdp.contains("m=audio"), "SDP must advertise audio");
    assert!(
        log.sdp.contains("packetization-mode=1"),
        "the sender fragments NAL units, so it must announce mode 1"
    );

    let video = video_packets.lock().unwrap();
    let reports = video_reports.lock().unwrap();
    let audio = audio_packets.lock().unwrap();
    assert!(
        video.len() > 100,
        "four seconds at 30fps should be hundreds of packets, got {}",
        video.len()
    );
    assert!(
        !audio.is_empty(),
        "audio should be flowing over the second RTP port, got {} packets", audio.len()
    );

    // --- video structure -------------------------------------------------
    let mut ssrc = None;
    let mut expected_seq: Option<u16> = None;
    let mut gaps = 0u32;
    let mut lost = 0u64;
    let mut worst_gap = 0u64;
    let mut markers = BTreeMap::new();
    for packet in video.iter() {
        assert_eq!(packet[0] >> 6, 2, "RTP version");
        assert_eq!(packet[1] & 0x7F, VIDEO_PAYLOAD_TYPE, "video payload type");
        assert!(packet.len() <= 12 + 1400, "packet exceeds the MTU budget");
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        let ts = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        let id = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);
        ssrc = Some(ssrc.unwrap_or(id));
        assert_eq!(ssrc, Some(id), "one SSRC for the whole stream");
        expected_seq = Some(match expected_seq {
            None => seq.wrapping_add(1),
            Some(next) => {
                if next != seq {
                    let dropped = seq.wrapping_sub(next) as u64;
                    gaps += 1;
                    lost += dropped;
                    worst_gap = worst_gap.max(dropped);
                }
                seq.wrapping_add(1)
            }
        });
        if packet[1] & 0x80 != 0 {
            *markers.entry(ts).or_insert(0u32) += 1;
        }
    }
    assert_eq!(
        (gaps, lost, worst_gap),
        (0, 0, 0),
        "loopback must not lose RTP packets ({} gaps, {} packets lost, worst burst {})",
        gaps,
        lost,
        worst_gap
    );

    let aus = group_by_timestamp(&video);
    let timestamps: Vec<u32> = aus.keys().copied().collect();
    println!(
        "video: {} packets -> {} access units, {} RTCP reports",
        video.len(),
        aus.len(),
        reports.len()
    );
    assert!(
        aus.len() >= 40,
        "four seconds should hold far more than {} pictures",
        aus.len()
    );

    let first = &aus[&timestamps[0]];
    let types: Vec<u8> = first.iter().map(|n| n[0] & 0x1F).collect();
    assert!(
        types.contains(&7) && types.contains(&8) && types.contains(&5),
        "the leading access unit must carry SPS, PPS and an IDR, got {types:?}"
    );

    // A picture may span several slice NALs; what must hold is that every
    // timestamp group is a complete picture with exactly one end-of-marker.
    let mut slice_total = 0usize;
    for (ts, nals) in &aus {
        let slices = nals
            .iter()
            .filter(|n| matches!(n[0] & 0x1F, 1 | 5))
            .count();
        assert!(slices >= 1, "AU at ts {ts} held no slice");
        assert_eq!(markers.get(ts), Some(&1), "exactly one marker per AU");
        slice_total += slices;
    }
    assert!(slice_total >= aus.len(), "every AU needs a slice");

    let deltas: Vec<i64> = timestamps
        .windows(2)
        .map(|w| w[1] as i64 - w[0] as i64)
        .collect();
    let mean = deltas.iter().sum::<i64>() as f64 / deltas.len() as f64;
    println!(
        "RTP timestamp spacing: mean {mean:.0} ticks (ideal {} per frame at 30fps)",
        VIDEO_CLOCK / 30
    );
    assert!(
        (mean - (VIDEO_CLOCK / 30) as f64).abs() < 600.0,
        "timestamps must track the 30fps grid, mean was {mean}"
    );
    assert!(deltas.iter().all(|d| *d > 0), "timestamps must increase");

    // --- RTCP ------------------------------------------------------------
    assert!(!reports.is_empty(), "the receiver should get sender reports");
    let sr = &reports[0];
    assert_eq!(sr[0] >> 6, 2, "RTCP version");
    assert_eq!(sr[1], 200, "sender report payload type");
    assert_eq!(
        u32::from_be_bytes([sr[4], sr[5], sr[6], sr[7]]),
        ssrc.unwrap(),
        "the report must come from the video SSRC"
    );

    // --- audio -----------------------------------------------------------
    let mut prev_audio_ts = None;
    let mut audio_frames = 0usize;
    for packet in audio.iter() {
        assert_eq!(packet[1] & 0x7F, AUDIO_PAYLOAD_TYPE, "audio payload type");
        let payload = &packet[12..];
        assert_eq!(
            &payload[..2],
            &[0x00, 0x10],
            "AAC-hbr starts with a 16-bit AU-headers length"
        );
        let size = ((payload[2] as usize) << 5) | ((payload[3] as usize) >> 3);
        assert_eq!(
            size,
            payload.len() - 4,
            "the AU size must describe the AAC frame it carries"
        );
        let ts = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        if let Some(prev) = prev_audio_ts {
            assert!(ts > prev, "audio timestamps must advance");
        }
        prev_audio_ts = Some(ts);
        audio_frames += 1;
    }
    println!("audio: {audio_frames} packets, {} RTCP reports", audio_reports.lock().unwrap().len());

    // --- session stats ---------------------------------------------------
    let stats = session.stats();
    println!(
        "session stats: {} frames captured, {} dropped, {} units sent, {} audio frames, {:.1} fps, {:.1} Mbps, {:.1} ms latency, {}",
        stats.frames_captured,
        stats.frames_dropped,
        stats.access_units_sent,
        stats.audio_frames_sent,
        stats.fps,
        stats.bitrate_mbps,
        stats.latency_ms,
        stats.resolution
    );
    assert_eq!(stats.error, None, "session reported an error: {:?}", stats.error);
    assert!(stats.frames_captured > 0, "frames should have been captured");
    assert!(stats.access_units_sent > 0, "access units should have been sent");
    assert!(stats.audio_frames_sent > 0, "audio frames should have been sent");
    assert!(
        stats.latency_ms > 0.0 && stats.latency_ms < 400.0,
        "measured latency {:.1} ms is outside a plausible range",
        stats.latency_ms
    );
    // The backlog cap is the reason a software encoder that cannot keep up
    // costs dropped frames rather than seconds of delay: what sits between
    // capture and output has to stay near the cap.
    let backlog = stats.frames_captured.saturating_sub(stats.access_units_sent);
    assert!(
        backlog <= 8,
        "{} frames fed but only {} access units sent: {} frames in flight",
        stats.frames_captured,
        stats.access_units_sent,
        backlog
    );
    assert!(stats.resolution.contains('x'), "resolution {stats:?}");

    // The fixture owns its process, whatever the assertions did on the way.
    let mut tone = tone;
    let _ = tone.kill();
    let _ = tone.wait();
}
