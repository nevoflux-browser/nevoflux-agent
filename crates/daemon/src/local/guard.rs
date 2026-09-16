//! `--engine-guard`: a tiny watchdog subprocess that prevents an installed
//! engine (`llama-server`) from surviving the daemon that launched it.
//!
//! Why this exists (v3 §16.2 / decision D10): Windows gets orphan prevention
//! for free from the daemon's kill-on-close Job Object
//! (`assign_self_to_kill_on_close_job` in `src/main.rs`) — any child spawned
//! after the daemon joins that Job dies when the Job's last handle closes.
//! Unix has no equivalent primitive reachable from safe, dependency-light
//! Rust, and the daemon's own shutdown path ends in `force_exit`
//! (`std::process::exit` on Unix) precisely because graceful async teardown
//! can wedge — so `Drop`-based cleanup (e.g. tokio's `Child::kill_on_drop`)
//! is not a reliable backstop: a hung shutdown, a SIGKILL, or a crash never
//! runs it.
//!
//! Instead, the daemon spawns the engine as `nevoflux-agent --engine-guard
//! -- <program> <args…>` with its own stdin piped from the daemon. This
//! process (not the daemon) directly parents the engine, in its own process
//! group. It blocks reading stdin: as long as the daemon is alive it holds
//! the write end open, so the read blocks forever. When the daemon dies —
//! by any means, since the OS unconditionally closes a dead process's file
//! descriptors — the pipe's write end closes and the guard's read returns
//! EOF (or, if the pipe itself errors, an `Err`). Either signals "the parent
//! is gone": the guard SIGTERMs the engine's whole process group, waits up
//! to 3 seconds, then SIGKILLs it. If the engine exits on its own first
//! (normal stop, crash), the guard simply relays its exit code and exits.
//!
//! `PR_SET_PDEATHSIG(SIGKILL)` is set on the guard itself as a Linux-only
//! backstop for the guard process's own survival (see [`run`]'s Linux
//! branch) — belt and braces on top of the stdin-EOF watch, which is the
//! primary, cross-Unix mechanism and works even on macOS, where `prctl`
//! does not exist.
//!
//! Not started at daemon startup; a later task (the engine supervisor)
//! decides when to spawn this.

/// `nevoflux-agent --engine-guard -- <program> <args…>` (Unix). Spawns the program in its own
/// process group, then blocks reading stdin; EOF (parent gone) or read error → SIGTERM the group,
/// 3s, SIGKILL. Exits with the child's code when the child exits first. On Windows: exit 2.
///
/// This dispatcher itself has no `#[cfg(unix)]` so `nevoflux-agent`'s `main()` can call it
/// unconditionally on every platform (see `src/main.rs`, right before `Cli::parse()`). The real
/// implementation lives in [`run_unix`], which is Unix-only; non-Unix targets get
/// [`run_unsupported`].
pub fn run(args: Vec<String>) -> ! {
    #[cfg(unix)]
    {
        run_unix(args)
    }
    #[cfg(not(unix))]
    {
        run_unsupported(args)
    }
}

#[cfg(not(unix))]
fn run_unsupported(_args: Vec<String>) -> ! {
    eprintln!("nevoflux-agent --engine-guard is only supported on Unix");
    std::process::exit(2);
}

/// One of the two events the guard races on: the child it is watching over
/// exited on its own, or its stdin (the daemon's liveness pipe) closed.
#[cfg(unix)]
enum Event {
    ChildExited(std::process::ExitStatus),
    StdinClosed,
}

/// Parse `<program> <args…>` out of the guard's argv, which looks like
/// `["--", "<program>", "<arg>", ...]` (everything after `--engine-guard` on
/// the outer command line). Searches for `--` rather than assuming it is
/// `args[0]` so future guard-specific flags ahead of it are not precluded.
#[cfg(unix)]
fn parse_args(args: &[String]) -> Option<(&str, &[String])> {
    let sep = args.iter().position(|a| a == "--")?;
    let rest = &args[sep + 1..];
    let (program, prog_args) = rest.split_first()?;
    Some((program.as_str(), prog_args))
}

/// Turn a child's exit status into a process exit code: its own code, or
/// (terminated by signal) the conventional `128 + signal` value.
#[cfg(unix)]
fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1)
}

/// Send a signal to the whole process group led by `pgid` (i.e. `pgid` is
/// also the group leader's own pid, which is how `Command::process_group(0)`
/// sets things up in [`run_unix`]). Best-effort: the group may already be
/// gone (ESRCH), which is not an error worth reporting here.
#[cfg(unix)]
fn kill_group(pgid: i32, sig: nix::sys::signal::Signal) {
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(-pgid), sig);
}

#[cfg(unix)]
fn run_unix(args: Vec<String>) -> ! {
    let Some((program, prog_args)) = parse_args(&args) else {
        eprintln!("nevoflux-agent --engine-guard: usage: --engine-guard -- <program> [args...]");
        std::process::exit(2);
    };

    // Belt-and-braces backstop for the guard process itself: if this
    // process's own parent (the daemon) dies, ask the kernel to SIGKILL us
    // directly. `PR_SET_PDEATHSIG` is Linux-only — `nix::sys::prctl` does
    // not exist on macOS (there is no direct equivalent there), so this is
    // gated stricter than the rest of this function. The stdin-EOF watch
    // below is the actual, cross-Unix mechanism that protects the *engine*
    // child; this only protects the guard from being orphaned itself.
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL) {
            eprintln!(
                "nevoflux-agent --engine-guard: prctl(PR_SET_PDEATHSIG) failed: {e} \
                 (continuing; the stdin-EOF watch is still active)"
            );
        }
    }

    let mut cmd = std::process::Command::new(program);
    cmd.args(prog_args);
    // The guard's own stdin is the daemon's liveness pipe — the child gets
    // none of it.
    cmd.stdin(std::process::Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        // pgroup 0: the child becomes the leader of a brand-new process
        // group (its pgid equals its own pid), so `kill_group` below can
        // reach it and everything it spawns without touching the guard.
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nevoflux-agent --engine-guard: failed to spawn {program}: {e}");
            std::process::exit(2);
        }
    };
    let pgid = child.id() as i32;

    let (tx, rx) = std::sync::mpsc::channel::<Event>();

    // Waiter thread: blocks on the child's exit. Plain std::thread, not
    // tokio — the guard must not start an async runtime.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            if let Ok(status) = child.wait() {
                let _ = tx.send(Event::ChildExited(status));
            }
        });
    }

    // Stdin watcher: blocks reading the daemon's liveness pipe. EOF (daemon
    // closed its end, e.g. it died) or any read error both mean "the parent
    // is gone" — either way we stop reading and report once.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 64];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            let _ = tx.send(Event::StdinClosed);
        });
    }
    // Drop our own sender so `rx` hangs up only once both threads above
    // have also dropped theirs (i.e. never on its own — each thread holds a
    // clone for as long as it can still send).
    drop(tx);

    match rx.recv() {
        // The child exited on its own before the parent went away — just
        // relay its exit code, nothing to kill.
        Ok(Event::ChildExited(status)) => std::process::exit(exit_code_of(status)),

        // The parent is gone. Escalate: SIGTERM the group, give it 3s,
        // SIGKILL if it's still around.
        Ok(Event::StdinClosed) => {
            use nix::sys::signal::Signal;
            kill_group(pgid, Signal::SIGTERM);
            match rx.recv_timeout(std::time::Duration::from_secs(3)) {
                Ok(Event::ChildExited(status)) => std::process::exit(exit_code_of(status)),
                _ => {
                    kill_group(pgid, Signal::SIGKILL);
                    // SIGKILL cannot be caught or blocked, so the waiter
                    // thread will report almost immediately; still fall
                    // back to a signal-convention exit code if it somehow
                    // never does.
                    match rx.recv() {
                        Ok(Event::ChildExited(status)) => std::process::exit(exit_code_of(status)),
                        // Should not happen — SIGKILL can't be caught or
                        // blocked — but fall back to the signal-convention
                        // code rather than hang if it somehow does.
                        _ => std::process::exit(128 + Signal::SIGKILL as i32),
                    }
                }
            }
        }

        // Both threads' senders dropped without ever sending — should not
        // happen (each always sends before returning), but don't hang.
        Err(_) => std::process::exit(1),
    }
}
