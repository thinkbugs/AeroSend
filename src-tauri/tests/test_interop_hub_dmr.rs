//! Interop: this app's control point against the HORIZON hub's DLNA renderer.
//!
//! The peer is not a test fixture. It is `dlna-probe` from the hub repo — the
//! same `MediaRenderer:1` description, the same SOAP services and, behind
//! them, the real `MediaEngineTransport` that decodes what it fetches. So this
//! run exercises our XML, HTTP, SOAP and byte-range server against code that
//! was written by a different project and cannot be wrong in the same way.
//!
//! It is `#[ignore]`d because it needs the peer alive:
//!
//! ```text
//! # in D:\codeX\meeting — leave this running for a few minutes
//! cargo run --release --bin dlna-probe -- --seconds=240 --port=8290 --headless
//!
//! # in D:\codeX\AeroSend\src-tauri
//! cargo test --test test_interop_hub_dmr -- --ignored --nocapture --test-threads=1
//! ```

use aerosend::upnp::{avt, device, Caster};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Where the hub renderer was started.
const HUB: &str = "127.0.0.1:8290";
const RATE: usize = 44_100;
const CLIP_SECONDS: usize = 4;

/// A PCM sine as a `.wav`: the hub's media engine will really decode it, so the
/// duration and clock it reports are its own measurement of our stream.
fn tone_wav() -> PathBuf {
    let samples = RATE * CLIP_SECONDS;
    let data = samples * 2;
    let mut bytes = Vec::with_capacity(44 + data);
    fn u32le(out: &mut Vec<u8>, v: usize) {
        out.extend_from_slice(&(v as u32).to_le_bytes());
    }
    fn u16le(out: &mut Vec<u8>, v: usize) {
        out.extend_from_slice(&(v as u16).to_le_bytes());
    }
    bytes.extend_from_slice(b"RIFF");
    u32le(&mut bytes, 36 + data);
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    u32le(&mut bytes, 16);
    u16le(&mut bytes, 1); // PCM
    u16le(&mut bytes, 1); // mono
    u32le(&mut bytes, RATE);
    u32le(&mut bytes, RATE * 2);
    u16le(&mut bytes, 2);
    u16le(&mut bytes, 16);
    bytes.extend_from_slice(b"data");
    u32le(&mut bytes, data);
    for i in 0..samples {
        let t = i as f64 / RATE as f64;
        let s = (t * std::f64::consts::TAU * 440.0).sin() * 12_000.0;
        u16le(&mut bytes, s as i16 as u16 as usize);
    }

    let mut path = std::env::temp_dir();
    path.push(format!("aerosend-interop-{}.wav", std::process::id()));
    std::fs::write(&path, &bytes).unwrap();
    path
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

/// The peer's description, with the instruction to start it if it is absent.
fn hub_renderer() -> device::Renderer {
    device::describe_by_address(HUB).unwrap_or_else(|e| {
        panic!(
            "no renderer at {HUB} ({e}): start the hub with `cargo run --release --bin \
             dlna-probe -- --seconds=240 --port=8290 --headless` in D:\\codeX\\meeting"
        )
    })
}

/// Register the hub with a caster the way the UI's "add by address" does.
fn connect(caster: &Caster) -> String {
    caster
        .add_by_location(HUB)
        .expect("the hub answers a description request")
        .id
}

#[test]
#[ignore = "needs the hub renderer alive; see the module doc comment"]
fn the_hub_fetches_decodes_and_answers_our_control() {
    let hub = hub_renderer();
    println!(
        "peer    {}  {}",
        hub.name,
        hub.control_url(device::AV_TRANSPORT).unwrap_or("?")
    );
    let av = hub.control_url(device::AV_TRANSPORT).unwrap().to_string();
    let rc = hub.control_url(device::RENDERING_CONTROL).unwrap().to_string();

    let caster = Caster::new();
    let id = connect(&caster);
    assert!(
        caster.renderers().iter().any(|r| r.castable),
        "the hub must look castable to us"
    );

    // 1. Hand it a file we serve, and let its own decoder pull it.
    let path = tone_wav();
    let status = caster
        .cast_file(&id, &path)
        .expect("casting a WAV the hub can decode");
    let offered = caster.offered()[0].clone();
    println!(
        "offered {} ({} bytes) as {}",
        offered.title, offered.size, offered.url
    );
    assert_eq!(offered.mime, "audio/wav", "a WAV is announced as one");
    assert_eq!(status.state, "PLAYING");

    // 2. The bytes crossed our HTTP server, on the hub's schedule.
    wait_for("the hub to fetch the file", Duration::from_secs(10), || {
        caster.bytes_served() > 0
    });
    println!("served  {} bytes so far", caster.bytes_served());

    // 3. Its clock, not ours: the device reports a duration and a position that
    //    moves while it decodes.
    wait_for("a position past zero on the device", Duration::from_secs(10), || {
        avt::position_info(&av).map(|p| p.seconds > 0.0).unwrap_or(false)
    });
    let device_says = avt::position_info(&av).unwrap();
    println!(
        "clock   {:.2}s of {:.2}s as the hub measures it",
        device_says.seconds, device_says.total_seconds
    );
    assert!(
        device_says.total_seconds > 3.0 && device_says.total_seconds < 5.0,
        "the hub decoded {} seconds of the 4-second clip we served",
        device_says.total_seconds
    );
    wait_for("our UI status to catch the device clock", Duration::from_secs(5), || {
        caster.status(&id).map(|s| s.total_seconds > 3.0).unwrap_or(false)
    });

    // 4. Transport control through the product's own commands.
    let paused = caster.transport_action(&id, "Pause").unwrap();
    assert_eq!(paused.state, "PAUSED_PLAYBACK", "{}", paused.state);
    let resumed = caster.transport_action(&id, "Play").unwrap();
    assert_eq!(resumed.state, "PLAYING");

    caster.seek(&id, 2.0).expect("seek to 2s");
    wait_for("the hub to be past the seek", Duration::from_secs(6), || {
        avt::position_info(&av).map(|p| p.seconds >= 1.5).unwrap_or(false)
    });

    // 5. Volume is the other service, and the device keeps it.
    caster.set_volume(&id, 25).expect("SetVolume on the hub");
    assert_eq!(avt::get_volume(&rc).unwrap(), 25, "the hub stored our value");

    // 6. What the hub advertises it can play — read from the device, not
    //    assumed. GetProtocolInfo is ConnectionManager's, not AVTransport's.
    let cm = hub
        .control_url(device::CONNECTION_MANAGER)
        .expect("the hub lists a ConnectionManager service")
        .to_string();
    let protocols = avt::sink_protocols(&cm).expect("GetProtocolInfo on the hub");
    println!("sinks   {} protocol entries", protocols.len());
    for p in protocols.iter().filter(|p| p.contains("audio")) {
        println!("        {p}");
    }
    assert!(!protocols.is_empty(), "a renderer that lists nothing is not usable");

    // 7. A scan must not cost us the renderer the user typed in.
    caster.scan();
    assert!(
        caster.renderers().iter().any(|r| r.id == id),
        "the pinned hub survived a real scan"
    );

    // 8. Stop ends it on the device as well as in our table.
    let stopped = caster.transport_action(&id, "Stop").unwrap();
    assert_eq!(stopped.state, "STOPPED");
    assert!(!stopped.active);
    assert_eq!(
        avt::transport_info(&av).unwrap().current,
        "STOPPED",
        "the hub agrees the session ended"
    );
    let served = caster.bytes_served();
    assert!(
        served >= offered.size as u64,
        "the hub read the whole {}-byte file; we sent {served}",
        offered.size
    );

    assert!(caster.remove(&id));
    assert!(caster.renderers().is_empty());

    caster.shutdown();
    std::fs::remove_file(&path).ok();
    println!("interop ok: hub {HUB} served, decoded, driven and stopped");
}

/// A clip ffmpeg makes in a second, so the length the hub reports is something
/// we know rather than something we read off the internet.
fn sample_mp4() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push("aerosend-interop.mp4");
    if !path.is_file() {
        panic!(
            "no sample at {}: make one with\n  ffmpeg -y -f lavfi -i \
             testsrc=duration=6:size=640x360:rate=25 -f lavfi -i \
             \"sine=frequency=440:duration=6\" -c:v libx264 -profile:v baseline -pix_fmt \
             yuv420p -c:a aac -shortest <temp>\\aerosend-interop.mp4",
            path.display()
        );
    }
    path
}

#[test]
#[ignore = "needs the hub renderer alive; see the module doc comment"]
fn an_mp4_cast_to_the_hub_is_decoded_and_driven() {
    let hub = hub_renderer();
    let av = hub.control_url(device::AV_TRANSPORT).unwrap().to_string();
    let caster = Caster::new();
    let id = connect(&caster);

    let path = sample_mp4();
    let size = std::fs::metadata(&path).unwrap().len();
    let status = caster
        .cast_file(&id, &path)
        .expect("the hub takes an MP4 it declared it can play");
    let offered = caster.offered()[0].clone();
    println!(
        "offered {} ({} bytes) as {} — mime {}",
        offered.title, offered.size, offered.url, offered.mime
    );
    assert_eq!(offered.mime, "video/mp4");
    assert_eq!(offered.size, size);
    assert_eq!(status.state, "PLAYING");

    // The hub's decoder, not our guess: it has to read the container to answer
    // with a duration, so a moving clock proves the bytes arrived and parsed.
    wait_for("the hub to start decoding", Duration::from_secs(15), || {
        avt::position_info(&av).map(|p| p.seconds > 0.0).unwrap_or(false)
    });
    let clock = avt::position_info(&av).unwrap();
    println!(
        "clock   {:.2}s of {:.2}s as the hub measures it",
        clock.seconds, clock.total_seconds
    );
    assert!(
        clock.total_seconds > 5.5 && clock.total_seconds < 6.5,
        "the hub read a {}-second clip out of the bytes we served",
        clock.total_seconds
    );

    let paused = caster.transport_action(&id, "Pause").unwrap();
    assert_eq!(paused.state, "PAUSED_PLAYBACK");
    // A paused cast must not keep moving.
    let frozen = avt::position_info(&av).unwrap().seconds;
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        (avt::position_info(&av).unwrap().seconds - frozen).abs() < 1.0,
        "a pause holds the clock at {frozen:.2}s"
    );

    caster.seek(&id, 3.0).expect("seek to 3s");
    wait_for("the hub to be past the seek", Duration::from_secs(8), || {
        avt::position_info(&av).map(|p| p.seconds >= 2.5).unwrap_or(false)
    });

    let stopped = caster.transport_action(&id, "Stop").unwrap();
    assert_eq!(stopped.state, "STOPPED");
    assert!(
        caster.bytes_served() >= size,
        "the whole clip crossed: {} of {size} bytes",
        caster.bytes_served()
    );
    assert!(caster.remove(&id));
    caster.shutdown();
    println!("interop ok: hub {HUB} decoded and drove an MP4");
}
