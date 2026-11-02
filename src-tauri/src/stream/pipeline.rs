//! Screen capture → H.264 encode → access units, shared by the mirroring path.
//!
//! RAOP (`stream::session`) is an audio endpoint and does not use this pump; the
//! AirPlay 2 mirroring path (`stream::mirror`) does.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::airplay::h264::{AccessUnitReader, Nal};
use crate::capture::encoder::VideoEncoder;
use crate::capture::screen::ScreenCapture;
use crate::stream::session::StreamStats;

/// Frames allowed between capture and encoder output: the pictures the encoder
/// still holds plus the one the access-unit grouper keeps back. Below the
/// encoder's own fill the gate would starve it and nothing would ever come out;
/// above it the viewer is watching history. Six frames is ~200ms at 30fps.
pub(crate) const MAX_BACKLOG_FRAMES: usize = 6;

/// An access unit plus the wall-clock time its frame left the screen capture.
pub(crate) struct AccessUnit {
    pub(crate) nalus: Vec<Nal>,
    pub(crate) captured_at: Instant,
}

/// Capture instants of fed frames, consumed in encoder output order. The
/// encoder preserves frame order and runs at zerolatency, so one queued
/// instant corresponds to one emitted access unit.
pub(crate) type CaptureClock = Arc<Mutex<VecDeque<Instant>>>;

/// Reads ffmpeg stdout, splits Annex-B, and emits grouped access units.
pub(crate) fn read_access_units(
    mut stdout: impl Read,
    running: &AtomicBool,
    tx: &SyncSender<AccessUnit>,
    clock: &CaptureClock,
) {
    let mut reader = AccessUnitReader::new();
    let mut chunk = [0u8; 64 * 1024];

    // Each emitted picture consumes one capture instant: the clock is fed by
    // the capture loop in the same order the encoder produces pictures.
    let emit = |nalus: Vec<Nal>| -> bool {
        let captured_at = clock
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(Instant::now);
        tx.send(AccessUnit {
            nalus,
            captured_at,
        })
        .is_ok()
    };

    while running.load(Ordering::SeqCst) {
        match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for nalus in reader.push(&chunk[..n]) {
                    if !emit(nalus) {
                        return;
                    }
                }
            }
        }
    }

    for nalus in reader.finish() {
        if !emit(nalus) {
            return;
        }
    }
}

pub(crate) fn run_capture_loop(
    capture: &mut ScreenCapture,
    encoder: &mut VideoEncoder,
    clock: &CaptureClock,
    running: &AtomicBool,
    stats: &Arc<Mutex<StreamStats>>,
    fps: u32,
) {
    let frame_interval = Duration::from_micros(1_000_000 / fps as u64);
    let mut next_tick = Instant::now();
    // Last pixels out of the capture, reused frame after frame: an 8.3 MB BGRA
    // allocation per tick is a cost the pipeline can see, and the pixels are
    // still here when the screen has not changed.
    let mut buffer: Vec<u8> = Vec::new();
    let mut fed = 0u64;
    let mut dropped = 0u64;

    while running.load(Ordering::SeqCst) {
        let due = next_tick;
        next_tick = next_tick + frame_interval;
        if next_tick > Instant::now() {
            std::thread::sleep(next_tick.saturating_duration_since(Instant::now()));
        }

        // AcquireNextFrame returns the current desktop, accumulated updates
        // included, so one readback per tick is already the newest picture.
        // A tick with nothing new must not re-send the previous pixels: a
        // static screen is supposed to cost nothing, and padding the stream
        // with copies only makes the receiver do work that changes no picture.
        let fresh = match capture.capture_frame_into(0, &mut buffer) {
            Ok(fresh) => fresh,
            Err(e) => {
                warn!("Capture error: {e}");
                false
            }
        };
        if !fresh {
            continue;
        }
        // Frames handed to the encoder leave the pipeline one access unit at a
        // time, so the queue length is its backlog. Past a few frames the viewer
        // is watching history, so the newest pixels replace what is not encoded
        // yet instead of queueing behind it.
        if clock.lock().unwrap().len() >= MAX_BACKLOG_FRAMES {
            dropped += 1;
            stats.lock().unwrap().frames_dropped = dropped;
            continue;
        }
        if encoder.send_frame(&buffer).is_err() {
            warn!("Video encoder stdin closed after {fed} frames");
            return;
        }
        // Exactly one clock instant per fed frame keeps the latency measure
        // aligned with the access units the reader pops.
        clock.lock().unwrap().push_back(due);
        fed += 1;
        stats.lock().unwrap().frames_captured = fed;
    }
    info!("Capture pump fed {fed} frames, dropped {dropped}");
}
