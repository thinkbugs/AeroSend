//! Streams the screen to a real AirPlay receiver and prints the sender's
//! counters once a second.
//!
//! ```text
//! cargo run --release --example mirror_e2e -- 127.0.0.1:5000 10 [fps]
//! ```
//!
//! Comparing the same sender against different receivers separates
//! sender-side cost from receiver-side backpressure: `captured` stops advancing
//! when the socket write blocks, and `sent` lags `captured` when the encoder
//! backlog grows.

use aerosend::capture::encoder::EncoderType;
use aerosend::stream::mirror::MirrorStream;
use aerosend::stream::session::{Resolution, StreamSettings};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:5000".to_string());
    let secs: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let fps: u32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let (host, port) = addr
        .rsplit_once(':')
        .map(|(h, p)| (h.to_string(), p.parse().expect("port")))
        .expect("host:port");

    let stream = MirrorStream::new();
    let running = stream.running_flag();
    let settings = StreamSettings {
        fps,
        bitrate: 4_000_000,
        audio: false,
        resolution: Resolution::Native,
        encoder: EncoderType::Auto,
    };

    std::thread::scope(|scope| {
        scope.spawn(|| {
            if let Err(e) = stream.start(&host, port, &settings) {
                eprintln!("session failed: {e:#}");
            }
        });

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(secs) {
            std::thread::sleep(Duration::from_secs(1));
            let s = stream.stats();
            if let Some(error) = &s.error {
                eprintln!("[{:>4.1}s] error: {error}", start.elapsed().as_secs_f64());
                break;
            }
            println!(
                "[{:>4.1}s] captured {:>4} sent {:>4} dropped {:>3} | {:.1} fps {:.1} Mbps \
                 latency {:>5.0} ms | {}",
                start.elapsed().as_secs_f64(),
                s.frames_captured,
                s.access_units_sent,
                s.frames_dropped,
                s.fps,
                s.bitrate_mbps,
                s.latency_ms,
                s.resolution
            );
        }

        stream.stop();
        let deadline = Instant::now() + Duration::from_secs(10);
        while running.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    });

    let s = stream.stats();
    println!(
        "summary: captured {} sent {} dropped {} bytes {:.1} MB, last fps {:.1}, latency {:.0} ms",
        s.frames_captured,
        s.access_units_sent,
        s.frames_dropped,
        s.bytes_sent as f64 / 1_000_000.0,
        s.fps,
        s.latency_ms
    );
    if let Some(error) = &s.error {
        eprintln!("last error: {error}");
    }
}
