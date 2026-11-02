//! End-to-end loopback test: the real RAOP session — WASAPI capture, ffmpeg
//! resample, RTSP handshake, L16 RTP/RTCP send — driving a fake AirPlay speaker
//! inside this process. Everything below the wire is production code; everything
//! above it is a black-box receiver that only knows the published protocol.
//!
//! Needs a desktop session and an audio render device, because the session
//! captures the actual system audio.

use aerosend::capture::encoder::EncoderType;
use aerosend::stream::session::{AUDIO_PAYLOAD_TYPE, Resolution};
use aerosend::stream::{StreamSession, StreamSettings};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RUN_FOR: Duration = Duration::from_secs(4);
/// 352 stereo frames of 16-bit samples.
const L16_PACKET_BYTES: usize = 1408;
const L16_FRAMES_PER_PACKET: u64 = 352;

#[derive(Debug, Default)]
struct ReceiverLog {
    methods: Vec<String>,
    sdp: String,
    transport: String,
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

/// RTP wants an even port and RTCP the next odd one, and ephemeral binds are
/// not guaranteed to land on such a pair, so probe for a free one.
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

#[test]
fn session_delivers_airplay_audio_over_loopback() {
    // The session reports a degraded sub-path (e.g. no audio device) through
    // tracing, not through its Result, so the log has to be visible here.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")))
        .try_init();

    let listener = TcpListener::bind("127.0.0.1:0").expect("rtsp bind");
    let rtsp_port = listener.local_addr().unwrap().port();
    let (audio_rtp, audio_rtcp) = bind_pair();
    let audio_port = audio_rtp.local_addr().unwrap().port();

    let log = Arc::new(Mutex::new(ReceiverLog::default()));
    let recording = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(true));

    let audio_packets = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let audio_reports = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let collectors = [
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

        while let Some(request) = read_request(&mut reader) {
            server_log.lock().unwrap().methods.push(request.method.clone());

            match request.method.as_str() {
                "OPTIONS" => respond(&mut stream, request.cseq, &[("Public", "OPTIONS,ANNOUNCE,SETUP,RECORD,TEARDOWN,GET_PARAMETER")]),
                "ANNOUNCE" => {
                    server_log.lock().unwrap().sdp = request.body.clone();
                    respond(&mut stream, request.cseq, &[]);
                }
                "SETUP" => {
                    server_log.lock().unwrap().transport = request.transport.clone();
                    respond(
                        &mut stream,
                        request.cseq,
                        &[
                            (
                                "Transport",
                                &format!(
                                    "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;\
                                     control_port={};timing_port={};server_port={audio_port}",
                                    audio_port + 1,
                                    audio_port + 1
                                ),
                            ),
                            ("Session", "1910829328;timeout=30"),
                        ],
                    );
                }
                "RECORD" => {
                    server_recording.store(true, Ordering::SeqCst);
                    respond(&mut stream, request.cseq, &[]);
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
        vec!["OPTIONS", "ANNOUNCE", "SETUP", "RECORD", "TEARDOWN"],
        "RTSP dialogue"
    );
    println!("SDP announced:\n{}", log.sdp);
    println!("SETUP transport: {}", log.transport);
    assert!(log.sdp.contains("m=audio"), "SDP must advertise audio");
    assert!(
        log.sdp.contains("a=rtpmap:96 L16/44100/2"),
        "the speaker transport decodes L16 only"
    );
    assert!(!log.sdp.contains("m=video"), "RAOP carries no video");
    // The receiver refuses SETUP without these, so they are part of the wire
    // contract rather than decoration.
    assert!(
        log.transport.contains("client_port=")
            && log.transport.contains("control_port=")
            && log.transport.contains("timing_port="),
        "transport must name the client, control and timing ports: {}",
        log.transport
    );
    assert!(
        log.transport.contains("mode=record"),
        "a sender records into the speaker: {}",
        log.transport
    );

    let audio = audio_packets.lock().unwrap();
    let reports = audio_reports.lock().unwrap();
    assert!(
        audio.len() > 100,
        "four seconds at 44.1 kHz should be hundreds of packets, got {}",
        audio.len()
    );

    // --- audio structure -------------------------------------------------
    let mut ssrc = None;
    let mut expected_seq: Option<u16> = None;
    let mut gaps = 0u32;
    let mut lost = 0u64;
    let mut prev_ts = None;
    for packet in audio.iter() {
        assert_eq!(packet[0] >> 6, 2, "RTP version");
        assert_eq!(packet[1] & 0x7F, AUDIO_PAYLOAD_TYPE, "audio payload type");
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        let ts = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        let id = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);
        ssrc = Some(ssrc.unwrap_or(id));
        assert_eq!(ssrc, Some(id), "one SSRC for the whole stream");

        // L16 has no codec framing: the payload is exactly 352 stereo frames.
        assert_eq!(
            packet.len() - 12,
            L16_PACKET_BYTES,
            "one L16 packet carries 352 stereo 16-bit frames"
        );

        expected_seq = Some(match expected_seq {
            None => seq.wrapping_add(1),
            Some(next) => {
                if next != seq {
                    let dropped = seq.wrapping_sub(next) as u64;
                    gaps += 1;
                    lost += dropped;
                }
                seq.wrapping_add(1)
            }
        });

        if let Some(previous) = prev_ts {
            assert_eq!(
                ts.wrapping_sub(previous),
                L16_FRAMES_PER_PACKET as u32,
                "the RTP clock advances by the samples each packet carries"
            );
        }
        prev_ts = Some(ts);
    }
    assert_eq!(
        (gaps, lost),
        (0, 0),
        "loopback must not lose RTP packets ({gaps} gaps, {lost} packets lost)"
    );
    let last_tick = prev_ts.expect("at least one audio packet");
    println!(
        "audio: {} packets -> {} sample frames, {} RTCP reports",
        audio.len(),
        last_tick as u64 + L16_FRAMES_PER_PACKET,
        reports.len()
    );
    // Timestamps start at zero and step by 352, so the last one is exactly one
    // packet-frame behind the total.
    assert_eq!(
        last_tick as u64 + L16_FRAMES_PER_PACKET,
        audio.len() as u64 * L16_FRAMES_PER_PACKET
    );

    // --- RTCP ------------------------------------------------------------
    assert!(!reports.is_empty(), "the receiver should get sender reports");
    let sr = &reports[0];
    assert_eq!(sr[0] >> 6, 2, "RTCP version");
    assert_eq!(sr[1], 200, "sender report payload type");
    assert_eq!(
        u32::from_be_bytes([sr[4], sr[5], sr[6], sr[7]]),
        ssrc.unwrap(),
        "the report must come from the audio SSRC"
    );

    // --- session stats ---------------------------------------------------
    let stats = session.stats();
    println!(
        "session stats: {} audio sample frames, {:.1} Mbps, {} bytes, {}",
        stats.audio_frames_sent, stats.bitrate_mbps, stats.bytes_sent, stats.error.as_deref().unwrap_or("ok")
    );
    assert_eq!(stats.error, None, "session reported an error: {:?}", stats.error);
    assert!(stats.audio_frames_sent > 0, "audio frames should have been sent");
    assert_eq!(
        stats.audio_frames_sent % L16_FRAMES_PER_PACKET,
        0,
        "frames are counted in whole packets of 352"
    );
    assert!(stats.bytes_sent > 0, "no audio payload left this machine");

    // The fixture owns its process, whatever the assertions did on the way.
    let mut tone = tone;
    let _ = tone.kill();
    let _ = tone.wait();
}
