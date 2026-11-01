use aerosend::capture::audio::{AudioCapture, PollResult};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn peak_amplitude(pcm: &[u8], is_float: bool) -> f32 {
    let mut peak = 0.0f32;
    if is_float {
        pcm.chunks_exact(4).for_each(|c| {
            let v = f32::from_le_bytes([c[0], c[1], c[2], c[3]]).abs();
            if v > peak {
                peak = v;
            }
        });
    } else {
        pcm.chunks_exact(2).for_each(|c| {
            let v = (i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).abs();
            if v > peak {
                peak = v;
            }
        });
    }
    peak
}

fn spawn_audio_source(duration_secs: u64) -> std::process::Child {
    Command::new("ffplay")
        .args([
            "-nodisp",
            "-autoexit",
            "-loglevel",
            "quiet",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={}", duration_secs),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn ffplay audio source")
}

#[test]
fn test_wasapi_loopback_capture_with_real_audio() {
    let capture = AudioCapture::new().expect("WASAPI loopback init failed");
    let fmt = capture.format().clone();
    println!(
        "mix format: {}Hz {}ch {}bit float={} block_align={}",
        fmt.sample_rate, fmt.channels, fmt.bits_per_sample, fmt.is_float, fmt.block_align
    );

    let mut player = spawn_audio_source(5);
    std::thread::sleep(Duration::from_millis(700));

    capture.start().expect("capture start failed");

    let deadline = Instant::now() + Duration::from_secs(4);
    let mut total_bytes = 0usize;
    let mut silent_bytes = 0usize;
    let mut peak = 0.0f32;
    let mut chunks: Vec<PollResult> = Vec::new();

    while Instant::now() < deadline {
        if let Some(res) = capture.poll() {
            total_bytes += res.pcm.len();
            silent_bytes += res.silent_bytes;
            let p = peak_amplitude(&res.pcm, fmt.is_float);
            if p > peak {
                peak = p;
            }
            chunks.push(res);
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    capture.stop();
    let _ = player.kill();
    let _ = player.wait();

    let nominal = fmt.sample_rate as usize * fmt.block_align as usize;
    println!(
        "captured {} bytes ({} silent) in {} chunks; peak amplitude {:.4}; nominal mix rate {} B/s",
        total_bytes,
        silent_bytes,
        chunks.len(),
        peak,
        nominal
    );

    for res in &chunks {
        assert_eq!(
            res.pcm.len() % fmt.block_align as usize,
            0,
            "chunk is not a whole number of frames"
        );
    }

    assert!(total_bytes > 0, "no audio frames returned from loopback capture");
    assert!(
        peak > 0.05,
        "loopback captured silence only while a 440Hz tone was playing (peak={peak})"
    );
}
