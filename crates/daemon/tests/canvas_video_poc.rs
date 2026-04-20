//! Phase A PoC test suite.
//!
//! Task 1: binary resolution (this file — current coverage).
//! Task 8: PoC gate orchestrator scaffold (panics until Task 13 lands).
//! Task 16: PoC gate test body — requires running browser.
//!   Run: cargo test -p nevoflux-daemon --test canvas_video_poc \
//!            poc_gate -- --ignored --nocapture

use sha2::{Digest, Sha256};
#[allow(unused_imports)]
use std::time::Instant;

#[test]
fn test_resolve_ffmpeg_succeeds() {
    let path = nevoflux_daemon::canvas_video::ffmpeg::resolve_ffmpeg()
        .expect("ffmpeg resolution must succeed");
    assert!(path.exists(), "resolved ffmpeg binary must exist: {:?}", path);

    let output = std::process::Command::new(&path)
        .arg("-version")
        .output()
        .expect("ffmpeg -version should run");
    assert!(output.status.success(), "ffmpeg -version exit = {:?}", output.status);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ffmpeg version"), "stdout missing version line");
}

#[test]
fn test_frame_chunks_reassemble_in_order() {
    use nevoflux_daemon::canvas_video::frame_chunks::ChunkBuffer;

    let mut buf = ChunkBuffer::new();

    // Simulated 2-chunk frame for frame_idx=0
    let r1 = buf.add_chunk(0, 0, 2, false, vec![0xDE, 0xAD]);
    assert!(r1.is_none(), "partial chunk should not complete");

    let r2 = buf.add_chunk(0, 1, 2, true, vec![0xBE, 0xEF]);
    assert_eq!(r2, Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));

    // Frame 0 should be gone
    assert!(!buf.has_frame(0));
}

#[test]
fn test_frame_chunks_reassemble_out_of_order() {
    use nevoflux_daemon::canvas_video::frame_chunks::ChunkBuffer;

    let mut buf = ChunkBuffer::new();
    // Arrive out of order: chunk 1 first, chunk 0 second.
    let r1 = buf.add_chunk(5, 1, 2, true, vec![0xCC, 0xDD]);
    assert!(r1.is_none());
    let r2 = buf.add_chunk(5, 0, 2, false, vec![0xAA, 0xBB]);
    assert_eq!(r2, Some(vec![0xAA, 0xBB, 0xCC, 0xDD]));
}

#[test]
fn test_frame_chunks_rejects_mismatched_total() {
    use nevoflux_daemon::canvas_video::frame_chunks::ChunkBuffer;

    let mut buf = ChunkBuffer::new();
    let _ = buf.add_chunk(7, 0, 3, false, vec![0x01]);
    // Second chunk declares total=2 which contradicts first chunk's total=3.
    let r = buf.add_chunk(7, 1, 2, false, vec![0x02]);
    assert!(r.is_none(), "mismatched total silently rejected (or could panic; at minimum must not corrupt)");
}

/// Runs the PoC composition twice through the full pipeline and
/// verifies:
///   1. MP4 output SHA256 bytes are equal across the two runs.
///   2. Per-frame drawSnapshot timing median ≤ 150 ms.
///   3. Total render time for 150 frames (5s @ 30fps) ≤ 20 s.
///
/// This test requires a running NevoFlux browser with the render
/// extension loaded. It drives two renders and compares outputs.
#[tokio::test]
#[ignore]
async fn poc_gate_determinism_and_perf() {
    // Placeholder: actual orchestration lives in Phase B's render pipeline
    // test harness. For PoC we drive the same path the production service
    // will use, but in a stripped-down test harness.
    //
    // The harness must:
    //   - Spawn a test ZMQ bridge (or use running daemon's).
    //   - Connect to the live extension.
    //   - Issue canvas.video.render.start twice with identical input.
    //   - Collect MP4 outputs.
    //   - Assert SHA256 equality + perf thresholds.
    //
    // Implementation of this harness is covered in Task 13 (render
    // service). Before then the operator should do a manual PoC using
    // the following procedure:

    panic!(
        "PoC orchestrator requires Phase B render service (Task 13). \
         Run Task 13 first, then return to complete this test."
    );
}

/// Standalone SHA256 helper used by the orchestrator and any manual PoC runs.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}
