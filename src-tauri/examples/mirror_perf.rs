//! Measures the mirror pipeline one stage at a time, so a slow mirror can be
//! blamed on the stage that is actually slow.
//!
//! Run it while the screen is animating (the mirror only sees what changes):
//!
//! ```text
//! ffplay -f lavfi -i testsrc2=size=1920x1080:rate=60 -fs
//! cargo run --release --example mirror_perf
//! ```
//!
//! Stages, in the order the live session runs them:
//!
//! 1. DXGI acquire + staging readback, tight loop: the ceiling on how many
//!    distinct frames this machine can hand over per second.
//! 2. Encoder fed by those frames: whether ffmpeg keeps up with the capture.
//! 3. Encoder fed from memory: the encoder's own ceiling, no display involved.
//! 4. AES-CTR + TCP to a local socket that reads as fast as it can: whether the
//!    wire costs anything measurable. A slow *receiver* is a separate stage and
//!    shows up live as a blocked `send_frame`, not here.

use aerosend::airplay::h264::AccessUnitReader;
use aerosend::airplay::mirror::MirrorWriter;
use aerosend::capture::encoder::{EncoderType, VideoEncoder};
use aerosend::capture::screen::ScreenCapture;
use std::io::Read;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn stats(mut values: Vec<f64>) -> (f64, f64, f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |p: f64| values[((values.len() - 1) as f64 * p).round() as usize];
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    (mean, at(0.5), at(0.95), *values.last().unwrap())
}

fn report(label: &str, values: Vec<f64>) {
    let (mean, p50, p95, max) = stats(values);
    println!("  {label}: mean {mean:.2} ms, p50 {p50:.2}, p95 {p95:.2}, max {max:.2}");
}

/// A reader thread that counts access units as the session's reader would.
struct AuCounter {
    count: Arc<AtomicU64>,
    done: Arc<AtomicU64>,
}

fn spawn_au_reader(mut stdout: impl Read + Send + 'static) -> AuCounter {
    let count = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicU64::new(0));
    let c = count.clone();
    let d = done.clone();
    std::thread::spawn(move || {
        let mut reader = AccessUnitReader::new();
        let mut chunk = [0u8; 64 * 1024];
        while let Ok(n) = stdout.read(&mut chunk) {
            if n == 0 {
                break;
            }
            for _ in reader.push(&chunk[..n]) {
                c.fetch_add(1, Ordering::Relaxed);
            }
        }
        d.store(1, Ordering::Relaxed);
    });
    AuCounter { count, done }
}

fn main() {
    let capture_secs: f64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(6.0);
    let encoder_secs = capture_secs;

    println!("=== stage 1: DXGI acquire + readback (tight loop, timeout 0) ===");
    let mut capture = ScreenCapture::new().expect("DXGI capture");
    let (screen_w, screen_h) = (capture.width(), capture.height());
    println!("  screen {screen_w}x{screen_h}");

    let mut buffer: Vec<u8> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();
    let mut map_ms = Vec::new();
    let mut copy_ms = Vec::new();
    let mut frames = 0u64;
    let mut bytes = 0u64;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs_f64(capture_secs) {
        let at = Instant::now();
        match capture.capture_frame_into(0, &mut buffer) {
            Ok(true) => {
                frames += 1;
                bytes += buffer.len() as u64;
                map_ms.push(at.elapsed().as_secs_f64() * 1000.0);

                // The row copy is inside the capture call; timing the same
                // memcpy on its own says how much of it is CPU and how much is
                // the GPU readback.
                if scratch.len() != buffer.len() {
                    scratch.resize(buffer.len(), 0);
                }
                let copy_at = Instant::now();
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        buffer.as_ptr(),
                        scratch.as_mut_ptr(),
                        buffer.len(),
                    );
                }
                copy_ms.push(copy_at.elapsed().as_secs_f64() * 1000.0);
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("  capture error: {e:#}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "  {frames} frames in {secs:.2} s = {:.1} fps, {:.0} MB/s of pixels",
        frames as f64 / secs,
        bytes as f64 / secs / 1_000_000.0
    );
    report("capture call (acquire + GPU copy + readback + memcpy + cursor)", map_ms);
    report("the same bytes, plain memcpy", copy_ms);

    println!("=== stage 2: encoder fed by capture (real ingress) ===");
    let mut encoder = VideoEncoder::new(
        screen_w,
        screen_h,
        screen_w,
        screen_h,
        30,
        4_000_000,
        EncoderType::Auto,
    );
    encoder.start().expect("encoder start");
    let au_reader = spawn_au_reader(encoder.take_stdout().expect("stdout"));
    let mut write_ms = Vec::new();
    let mut fed = 0u64;
    let mut buffer: Vec<u8> = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs_f64(encoder_secs) {
        match capture.capture_frame_into(0, &mut buffer) {
            Ok(true) => {
                let at = Instant::now();
                if encoder.send_frame(&buffer).is_err() {
                    break;
                }
                write_ms.push(at.elapsed().as_secs_f64() * 1000.0);
                fed += 1;
            }
            Ok(false) => std::thread::sleep(Duration::from_millis(1)),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    encoder.stop().ok();
    while au_reader.done.load(Ordering::Relaxed) == 0 {
        std::thread::sleep(Duration::from_millis(10));
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "  fed {fed} frames in {secs:.2} s = {:.1}/s, encoder emitted {} access units",
        fed as f64 / secs,
        au_reader.count.load(Ordering::Relaxed)
    );
    report("stdin write (blocks when ffmpeg is behind)", write_ms);

    println!("=== stage 3: encoder ceiling, frames from memory ===");
    // Eight noise frames cycled: content changes every frame and is closer to
    // the worst case than a mostly static desktop.
    let frame_len = (screen_w * screen_h * 4) as usize;
    let mut pool: Vec<Vec<u8>> = Vec::new();
    let mut seed = 0x1234_5678u32;
    for _ in 0..8 {
        let mut frame = vec![0u8; frame_len];
        for chunk in frame.chunks_exact_mut(4) {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            chunk.copy_from_slice(&seed.to_le_bytes());
        }
        pool.push(frame);
    }
    let mut encoder = VideoEncoder::new(
        screen_w,
        screen_h,
        screen_w,
        screen_h,
        30,
        4_000_000,
        EncoderType::Auto,
    );
    encoder.start().expect("encoder start");
    let counter = spawn_au_reader(encoder.take_stdout().expect("stdout"));
    let count = 120u64;
    let start = Instant::now();
    let mut write_ms = Vec::new();
    for i in 0..count {
        let at = Instant::now();
        if encoder.send_frame(&pool[(i % pool.len() as u64) as usize]).is_err() {
            break;
        }
        write_ms.push(at.elapsed().as_secs_f64() * 1000.0);
    }
    let fed_secs = start.elapsed().as_secs_f64();
    encoder.stop().ok();
    while counter.done.load(Ordering::Relaxed) == 0 {
        std::thread::sleep(Duration::from_millis(10));
    }
    let total = start.elapsed().as_secs_f64();
    println!(
        "  {count} noise frames fed in {fed_secs:.2} s ({:.1} fps ingress), {} access units out, \
         last after {total:.2} s ({:.1} fps end-to-end)",
        count as f64 / fed_secs,
        counter.count.load(Ordering::Relaxed),
        counter.count.load(Ordering::Relaxed) as f64 / total
    );
    report("stdin write", write_ms);

    println!("=== stage 4: AES-CTR + TCP to a listening sink ===");
    let listener = TcpListener::bind("127.0.0.1:0").expect("sink listener");
    let addr = listener.local_addr().unwrap();
    let sink = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("sink accept");
        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        total
    });
    let mut writer = MirrorWriter::connect(
        &addr.to_string(),
        &[0x11u8; 16],
        &[0x22u8; 32],
        "123456789012",
    )
    .expect("mirror writer");
    // One second of frames at 4 Mbps, in 16 KiB payloads.
    let payload = vec![0x5au8; 16 * 1024];
    let nal = aerosend::airplay::h264::Nal {
        data: payload.clone(),
    };
    let nalus = vec![nal];
    let iterations = (4_000_000 / 8 / payload.len() as u64).max(1);
    let mut write_ms = Vec::new();
    let start = Instant::now();
    for _ in 0..iterations {
        let at = Instant::now();
        writer.write_video(&nalus, 0).expect("write_video");
        write_ms.push(at.elapsed().as_secs_f64() * 1000.0);
    }
    let secs = start.elapsed().as_secs_f64();
    drop(writer);
    let received = sink.join().expect("sink thread");
    println!(
        "  {iterations} pictures, {:?} bytes written in {secs:.2} s = {:.1} MB/s; sink read {received} bytes",
        payload.len() as u64 * iterations,
        payload.len() as f64 * iterations as f64 / secs / 1_000_000.0
    );
    report("one picture encrypted + written", write_ms);

    println!("done");
}
