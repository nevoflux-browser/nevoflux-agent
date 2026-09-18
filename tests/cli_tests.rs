//! CLI integration tests.
//!
//! These tests verify the command-line interface behavior of the nevoflux binary.

#![allow(deprecated)]

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn test_help_output() {
    Command::cargo_bin("nevoflux-agent")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("NevoFlux"));
}

#[test]
fn test_version_output() {
    Command::cargo_bin("nevoflux-agent")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"nevoflux-agent \d+\.\d+\.\d+").unwrap());
}

#[test]
fn test_status_not_running() {
    let temp = tempfile::TempDir::new().unwrap();

    Command::cargo_bin("nevoflux-agent")
        .unwrap()
        .arg("--status")
        .env("NEVOFLUX_DATA_DIR", temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("not running"));
}

#[test]
fn test_stop_when_not_running() {
    let temp = tempfile::TempDir::new().unwrap();

    Command::cargo_bin("nevoflux-agent")
        .unwrap()
        .arg("--stop")
        .env("NEVOFLUX_DATA_DIR", temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("not running"));
}

#[test]
fn test_status_with_stale_files() {
    let temp = tempfile::TempDir::new().unwrap();

    // Create stale daemon files
    std::fs::write(temp.path().join("daemon.port"), "19500").unwrap();
    std::fs::write(temp.path().join("daemon.pid"), "99999").unwrap();

    Command::cargo_bin("nevoflux-agent")
        .unwrap()
        .arg("--status")
        .env("NEVOFLUX_DATA_DIR", temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("stale files"));
}

#[test]
fn test_stop_cleans_stale_files() {
    let temp = tempfile::TempDir::new().unwrap();

    // Create stale daemon files
    let port_file = temp.path().join("daemon.port");
    let pid_file = temp.path().join("daemon.pid");
    std::fs::write(&port_file, "19500").unwrap();
    std::fs::write(&pid_file, "99999").unwrap();

    Command::cargo_bin("nevoflux-agent")
        .unwrap()
        .arg("--stop")
        .env("NEVOFLUX_DATA_DIR", temp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Cleanup complete"));

    // Verify files are cleaned
    assert!(!port_file.exists());
    assert!(!pid_file.exists());
}

// --engine-guard (Unix orphan prevention, v3 §16.2 / decision D10).
//
// NOTE: this test group is compiled out entirely on non-Unix targets
// (including this Windows dev machine) — it has not been executed here.
// It is written and confirmed to compile (`cargo check -p nevoflux-agent`);
// actual pass/fail is pending a Linux or macOS run.
#[cfg(unix)]
mod engine_guard {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Poll `kill -0 <pid>` (no signal sent, just existence/permission
    /// check) to ask whether `pid` still names a live process.
    fn process_alive(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Find a direct child of `ppid` via `pgrep -P`, polling briefly since
    /// the guard's fork+exec of its child is not instantaneous.
    fn find_child_pid(ppid: u32) -> Option<u32> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(out) = Command::new("pgrep")
                .args(["-P", &ppid.to_string()])
                .output()
            {
                if let Some(line) = String::from_utf8_lossy(&out.stdout).lines().next() {
                    if let Ok(pid) = line.trim().parse::<u32>() {
                        return Some(pid);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// Dropping the guard's stdin (standing in for the daemon dying, which
    /// closes the pipe the same way) must make the guard SIGTERM/SIGKILL
    /// the child's process group within the guard's 3s grace window.
    #[test]
    fn kills_child_group_when_parent_stdin_closes() {
        let bin = assert_cmd::cargo::cargo_bin!("nevoflux-agent");
        let mut guard = Command::new(bin)
            .args(["--engine-guard", "--", "sleep", "300"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn nevoflux-agent --engine-guard");

        let guard_pid = guard.id();
        let sleep_pid =
            find_child_pid(guard_pid).expect("guard should have spawned `sleep` as its child");
        assert!(
            process_alive(sleep_pid),
            "sleep {sleep_pid} should be running before the guard's stdin closes"
        );

        // Close the guard's stdin write end — the guard's "parent is gone" signal.
        drop(guard.stdin.take());

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !process_alive(sleep_pid) {
                let _ = guard.wait();
                return;
            }
            if Instant::now() >= deadline {
                let _ = guard.kill();
                let _ = guard.wait();
                panic!(
                    "sleep {sleep_pid} still alive 5s after the guard's stdin closed \
                     (guard pid {guard_pid})"
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// When the watched child exits on its own (parent still alive), the
    /// guard must relay its exact exit code rather than swallow it.
    #[test]
    fn relays_child_exit_code() {
        let bin = assert_cmd::cargo::cargo_bin!("nevoflux-agent");
        let mut guard = Command::new(bin)
            .args(["--engine-guard", "--", "sh", "-c", "exit 3"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn nevoflux-agent --engine-guard");

        // Hold the write end open ourselves, independent of `Child::wait`,
        // which closes `child.stdin` as its very first action ("The stdin
        // handle to the child process, if any, will be closed before
        // waiting" — std docs). Using `status()`/`wait()` here would race
        // the guard's own stdin-EOF watch against `sh` actually running:
        // the guard could observe EOF and SIGTERM the group before
        // `exit 3` ever executes, exercising the *kill* path (exit 143)
        // instead of the "child exited on its own" path this test is
        // about. Polling `try_wait()` instead keeps the write end alive
        // until the guard has genuinely already exited by itself.
        let _stdin = guard.stdin.take();

        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = guard.try_wait().expect("try_wait on the guard") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "guard did not exit within 5s over `sh -c \"exit 3\"`"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        drop(_stdin);

        assert_eq!(status.code(), Some(3));
    }
}
