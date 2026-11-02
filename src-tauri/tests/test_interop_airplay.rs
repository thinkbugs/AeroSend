//! Interop: this app's AirPlay sender against a real AirPlay receiver.
//!
//! The peer is not a fixture. It is the receiver inside 戎耀会投屏 / HORIZON
//! 会议白板启动器 — its own `_airplay._tcp`/`_raop._tcp` advertisements, its own
//! RTSP server and, behind them, a decoder written by a different project. So
//! these runs check our SDP, SETUP negotiation and payloads against code that
//! cannot be wrong in the same way ours is.
//!
//! Two transports are covered:
//!
//! - RAOP (`_raop._tcp`): L16 audio. The receiver's speaker endpoint accepts
//!   the ANNOUNCE and SETUP only when the SDP is L16 and the transport names a
//!   control and timing port.
//! - AirPlay 2 mirroring (`_airplay._tcp` with `pk`): pair-verify, fp-setup and
//!   the stream 110 `SETUP`, then pictures down the TCP port the receiver
//!   named.
//!
//! It is `#[ignore]`d because it needs the peer alive:
//!
//! ```text
//! # start 戎耀会投屏 and leave it on the AirPlay mirror page
//!
//! # in D:\codeX\AeroSend\src-tauri
//! cargo test --test test_interop_airplay -- --ignored --nocapture --test-threads=1
//!
//! # skip discovery for every test and aim straight at the peer:
//! AEROSEND_PEER=192.168.1.23:7000 \
//!     cargo test --test test_interop_airplay -- --ignored --nocapture --test-threads=1
//! # or pick it out of the browse list by name:
//! AEROSEND_NAME=戎耀会 \
//!     cargo test --test test_interop_airplay -- --ignored --nocapture --test-threads=1
//! # a single test can take the peer on the command line instead:
//! cargo test --test test_interop_airplay -- --ignored --nocapture --test-threads=1 \
//!     a_real_airplay_speaker_takes_our_l16_audio -- --peer=127.0.0.1:5000
//! ```

use aerosend::airplay::discovery::DiscoveryService;
use aerosend::capture::encoder::EncoderType;
use aerosend::stream::mirror::MirrorStream;
use aerosend::stream::session::Resolution;
use aerosend::stream::{StreamSession, StreamSettings};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How long the session is held open once the receiver takes the handshake.
const STREAM_FOR: Duration = Duration::from_secs(6);
/// AirPlay receivers answer RTSP on 7000 by default; the port actually used
/// comes from the advertisement, not from this constant.
const DEFAULT_AIRPLAY_PORT: u16 = 7000;

fn argument(flag: &str) -> Option<String> {
    // libtest stops parsing at `--` and treats what follows as a name filter,
    // so an env var is the only way to give every test a peer in one run.
    if let Ok(value) = std::env::var(format!("AEROSEND_{}", flag.to_uppercase())) {
        return Some(value);
    }
    let prefix = format!("--{flag}=");
    std::env::args()
        .find(|a| a.starts_with(&prefix))
        .map(|a| a[prefix.len()..].to_string())
}

/// A tone on the default render device: WASAPI loopback only mirrors what is
/// actually playing, so an idle machine would prove nothing about the audio
/// path. The child is left to exit on its own after `duration_secs`.
fn play_a_tone(duration_secs: u64) {
    if let Err(e) = Command::new("ffplay")
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
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        println!("note: no audio source ({e}); the audio assertions will show it");
    }
}

fn wait_for(label: &str, timeout: Duration, cond: impl Fn() -> bool) {
    let start = Instant::now();
    while !cond() {
        if start.elapsed() > timeout {
            panic!("timed out after {:?} waiting for {label}", start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("[{:>6}ms] {label}", start.elapsed().as_millis());
}

/// `--peer=host[:port]` wins; otherwise browse mDNS for an AirPlay service.
fn choose_peer() -> (String, u16, String) {
    if let Some(address) = argument("peer") {
        let (host, port) = match address.split_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().expect("--peer=host:port")),
            None => (address.clone(), DEFAULT_AIRPLAY_PORT),
        };
        println!("peer    {host}:{port} (given by --peer or AEROSEND_PEER)");
        return (host, port, "manual".to_string());
    }

    let mut service = DiscoveryService::new().expect("mDNS browse");
    let _events = service.start();
    println!("browsing _airplay._tcp for 8 s …");
    std::thread::sleep(Duration::from_secs(8));
    let mut devices = service.get_devices();
    service.stop();

    devices.sort_by(|a, b| a.name.cmp(&b.name));
    for device in &devices {
        println!(
            "        {}  {}:{}  type={}",
            device.name, device.host, device.port, device.device_type
        );
    }

    let airplay: Vec<_> = devices
        .into_iter()
        .filter(|d| d.device_type == "airplay")
        .collect();
    let wanted = argument("name");
    let picked = airplay
        .iter()
        .find(|d| wanted.as_ref().is_none_or(|w| d.name.contains(w.as_str())))
        .unwrap_or_else(|| {
            panic!(
                "no AirPlay receiver answered mDNS{}. Start 戎耀会投屏, or pass \
                 --peer=host:port",
                wanted
                    .as_ref()
                    .map(|w| format!(" matching {w:?}"))
                    .unwrap_or_default()
            )
        });

    println!("peer    {}  {}:{}", picked.name, picked.host, picked.port);
    let features = picked
        .properties
        .get("features")
        .map(|f| format!(" features={f}"))
        .unwrap_or_default();
    println!(
        "        model={} srcvers={} ports={}",
        picked.properties.get("model").cloned().unwrap_or_default(),
        picked.properties.get("srcvers").cloned().unwrap_or_default(),
        picked.properties.keys().len()
    );
    (
        picked.host.clone(),
        picked.port,
        format!("{}{features}", picked.name),
    )
}

#[test]
#[ignore = "needs a real AirPlay speaker alive; see the module doc comment"]
fn a_real_airplay_speaker_takes_our_l16_audio() {
    let (host, port, label) = choose_peer();

    let session = StreamSession::new();
    let running = session.running_flag();
    let settings = StreamSettings {
        fps: 30,
        bitrate: 4_000_000,
        audio: true,
        resolution: Resolution::Native,
        encoder: EncoderType::Auto,
    };

    play_a_tone(20);

    std::thread::scope(|scope| {
        scope.spawn(|| session.start(&host, port, &settings));

        // The handshake is the interop claim: our SDP has to be announced and
        // SETUP'd by the peer, and it has to stay RECORDing us rather than
        // hanging up.
        wait_for("the speaker answers our RTSP handshake", Duration::from_secs(20), || {
            session.stats().error.is_some() || session.stats().audio_frames_sent > 0
        });

        let stats = session.stats();
        if let Some(error) = &stats.error {
            panic!("{label} rejected our audio: {error}");
        }

        println!("        {} audio frames already accepted", stats.audio_frames_sent);
        std::thread::sleep(STREAM_FOR);
        session.stop();
        wait_for("the session ends", Duration::from_secs(15), || {
            !running.load(Ordering::SeqCst)
        });
    });

    let stats = session.stats();
    println!(
        "sent    {} audio sample frames, {} RTP packets, {:.2} MB, {:.0} kbps",
        stats.audio_frames_sent,
        stats.rtp_packets_sent,
        stats.bytes_sent as f64 / 1_000_000.0,
        stats.bitrate_mbps * 1000.0
    );

    if let Some(error) = &stats.error {
        panic!("the session ended badly after a good handshake: {error}");
    }
    assert!(
        stats.audio_frames_sent > 0,
        "the speaker accepted the session but no audio frame was sent — check \
         that a tone was playing and that the audio device carried it: {stats:?}"
    );
    assert!(
        stats.rtp_packets_sent > 0 && stats.bytes_sent > 0,
        "no RTP audio left this machine"
    );
}

/// The transport the app picks comes from the advert, not from a setting, so a
/// receiver that offers a screen has to be recognisable as one from mDNS alone.
#[test]
#[ignore = "needs the receiver running and advertising on the LAN"]
fn the_mirroring_advert_is_recognisable_from_mdns() {
    let mut service = DiscoveryService::new().expect("mDNS browse");
    let _events = service.start();
    std::thread::sleep(Duration::from_secs(8));
    let devices = service.get_devices();
    service.stop();

    for device in &devices {
        println!(
            "        {:<24} {}:{}  type={}  mirroring={}",
            device.name,
            device.host,
            device.port,
            device.device_type,
            device.mirrors_screen()
        );
    }

    let screens: Vec<_> = devices.iter().filter(|d| d.mirrors_screen()).collect();
    assert!(
        !screens.is_empty(),
        "no advert published an identity, so every device would be sent RAOP \
         audio and the screen path would never start"
    );
}

/// The AirPlay 2 path against the same peer: pair-verify, fp-setup, the stream
/// 110 `SETUP`, encrypted pictures down the port it named, and — with audio on —
/// AAC-LC packets on the stream 96 sockets from the same handshake.
#[test]
#[ignore = "needs a real AirPlay 2 receiver on its mirror page; see the module doc comment"]
fn a_real_airplay_receiver_takes_our_mirroring_handshake() {
    let (host, port, label) = choose_peer();

    let stream = MirrorStream::new();
    let running = stream.running_flag();
    let settings = StreamSettings {
        fps: 30,
        bitrate: 4_000_000,
        audio: true,
        resolution: Resolution::Native,
        encoder: EncoderType::Auto,
    };

    // Mirrored audio is WASAPI loopback, so the machine has to be playing
    // something for there to be samples to encode.
    play_a_tone(20);

    std::thread::scope(|scope| {
        scope.spawn(|| stream.start(&host, port, &settings));

        // Every handshake step either lands or it does not, and the receiver
        // holding the data socket open long enough to take a picture is the only
        // proof that the keystreams agree.
        wait_for(
            "the receiver completes the AirPlay 2 handshake",
            Duration::from_secs(25),
            || {
                let stats = stream.stats();
                stats.error.is_some() || stats.access_units_sent > 0
            },
        );

        let stats = stream.stats();
        if let Some(error) = &stats.error {
            panic!("{label} refused the mirroring session: {error}");
        }
        println!(
            "        {} negotiated, {} picture(s) already on the data port",
            stats.resolution, stats.access_units_sent
        );
        wait_for("mirrored audio reaches the receiver's ports", Duration::from_secs(15), || {
            stream.stats().audio_frames_sent > 0
        });
        std::thread::sleep(STREAM_FOR);
        stream.stop();
        wait_for("the mirroring session ends", Duration::from_secs(15), || {
            !running.load(Ordering::SeqCst)
        });
    });

    let stats = stream.stats();
    println!(
        "sent    {} frames captured, {} dropped, {} pictures, {:.1} MB, {:.1} fps, \
         {:.1} Mbps, {:.1} ms latency",
        stats.frames_captured,
        stats.frames_dropped,
        stats.access_units_sent,
        stats.bytes_sent as f64 / 1_000_000.0,
        stats.fps,
        stats.bitrate_mbps,
        stats.latency_ms
    );
    println!(
        "audio   {} sample frames, {} RTP packets, {:.2} MB",
        stats.audio_frames_sent,
        stats.rtp_packets_sent,
        stats.audio_bytes_sent as f64 / 1_000_000.0
    );

    if let Some(error) = &stats.error {
        panic!("the mirroring session ended badly after a good handshake: {error}");
    }
    assert!(
        stats.frames_captured > 0,
        "nothing was captured to send: {stats:?}",
    );
    assert!(
        stats.access_units_sent > 0,
        "the receiver opened its data port but no picture reached it — the \
         keystreams do not agree: {stats:?}"
    );
    assert!(stats.bytes_sent > 0, "no bytes left this machine");
    assert!(
        stats.audio_frames_sent > 0,
        "the receiver took stream 96 but no audio frame was sent — check that a \
         tone was playing and that the AAC encoder opened: {stats:?}"
    );
    assert!(
        stats.audio_frames_sent % 1024 == 0,
        "audio frames must advance one AAC-LC access unit at a time: {stats:?}"
    );
}
