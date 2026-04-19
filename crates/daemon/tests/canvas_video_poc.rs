//! Phase A PoC test suite.
//!
//! Task 1: binary resolution (this file — current coverage).
//! Later tasks (8, 16) add drawSnapshot + encode round-trip tests.

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
