use aerosend::capture::screen::{ScreenCapture, get_primary_monitor_size};
use std::time::Instant;

#[test]
fn test_monitor_size() {
    let (w, h) = get_primary_monitor_size().expect("Failed to get monitor size");
    println!("Primary monitor: {}x{}", w, h);
    assert!(w > 0 && h > 0, "Monitor dimensions must be positive");
}

// One duplication per test process: DXGI rejects the second concurrent
// CreateDuplicateOutput on the same output, so init and frame pulling share a
// single capture object instead of racing as two tests.
#[test]
fn test_dxgi_capture() {
    let mut capture = ScreenCapture::new().expect("Failed to init DXGI capture");
    println!("DXGI capture: {}x{}", capture.width(), capture.height());
    assert!(capture.width() > 0 && capture.height() > 0);
    let mut frame_count = 0;
    let mut total_bytes = 0u64;
    let start = Instant::now();
    let test_duration = std::time::Duration::from_secs(3);

    println!("Capturing frames for 3 seconds...");

    while start.elapsed() < test_duration {
        match capture.capture_frame(16) {
            Ok(Some(frame)) => {
                frame_count += 1;
                total_bytes += frame.data.len() as u64;
                if frame_count == 1 {
                    println!("First frame: {}x{}, {} bytes, pitch={}",
                        frame.width, frame.height, frame.data.len(), frame.pitch);
                    let non_zero = frame.data.iter().filter(|&&b| b != 0).count();
                    let pct = non_zero as f64 / frame.data.len() as f64 * 100.0;
                    println!("Non-zero pixels: {}/{} ({:.1}%)", non_zero, frame.data.len(), pct);
                    assert!(frame.data.len() > 0, "Frame data must not be empty");
                    assert_eq!(frame.data.len() as u32, frame.width * frame.height * 4,
                        "Frame data size mismatch");
                }
            }
            Ok(None) => {}
            Err(e) => println!("Frame error: {}", e),
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    let elapsed = start.elapsed();
    let fps = frame_count as f64 / elapsed.as_secs_f64();
    let bandwidth = total_bytes as f64 / elapsed.as_secs_f64() / 1_000_000.0;

    println!("\nResults:");
    println!("  Frames: {}", frame_count);
    println!("  FPS: {:.1}", fps);
    println!("  Bandwidth: {:.1} MB/s", bandwidth);
    println!("  Total: {:.1} MB", total_bytes as f64 / 1_000_000.0);

    assert!(frame_count > 0, "Should capture at least one frame in 3 seconds");
}
