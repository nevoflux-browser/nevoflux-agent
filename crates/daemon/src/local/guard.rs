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
//! Only the child's stdin is redirected (to `/dev/null`) — its stdout and
//! stderr are inherited from the guard as-is. **Whatever spawns the guard
//! must therefore give it a non-protocol stdout/stderr**: this repo's
//! native-messaging channel uses the daemon's real stdout for protocol
//! bytes, and `llama-server` is chatty, so a spawn site that hands the
//! guard an inherited protocol stdout would corrupt that channel with
//! engine logs.
//!
//! Deliberately **not** using `PR_SET_PDEATHSIG` (Linux's `prctl` parent-death
//! signal) as a backstop for the guard's own survival, even though it looks
//! like a natural fit: the kernel arms it against the *thread* that called
//! `prctl`, not the parent *process* — so if a future caller ever spawns the
//! guard from a thread that can itself exit before the daemon does (a tokio
//! `spawn_blocking` worker, retired after a short idle window, is exactly
//! such a thread), the guard would be SIGKILLed out from under a perfectly
//! healthy daemon and engine. The engine, alone in its own process group
//! with no other watcher, would then be orphaned permanently — precisely
//! the D10 failure this module exists to prevent. The stdin-EOF watch below
//! has no such hazard (a process's file descriptors close when it dies, by
//! any means, unconditionally on the *process*, never the caller's thread),
//! so it is the only mechanism used here. Do not re-add `prctl`.
//!
//! Not started at daemon startup; a later task (the engine supervisor)
//! decides when to spawn this.

/// `nevoflux-agent --engine-guard -- <program> <args…>` (Unix). Spawns the program in its own
/// process group, then blocks reading stdin; EOF (parent gone) or read error → SIGTERM the group,
/// 3s, SIGKILL. Exits with the child's code when the child exits first. On Windows: exit 2.
///
/// `nevoflux-agent`'s `main()` calls this unconditionally on every platform (see `src/main.rs`,
/// right before `Cli::parse()`), so it exists in two complete, separately cfg'd definitions rather
/// than one body with a cfg'd block inside it — each definition is then an ordinary function whose
/// only statement is a plain tail call to a `-> !` function, with no attribute-on-block-statement
/// subtlety for the `-> !` return type to be inferred through.
#[cfg(unix)]
pub fn run(args: Vec<String>) -> ! {
    run_unix(args)
}

/// Non-Unix counterpart of the `#[cfg(unix)]` `run` above — see its doc comment.
#[cfg(not(unix))]
pub fn run(args: Vec<String>) -> ! {
    run_unsupported(args)
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

    // No `PR_SET_PDEATHSIG` here — see the module header for why it would be
    // a hazard rather than a backstop (it tracks the calling *thread*, not
    // the daemon process). The stdin-EOF watch below is the sole mechanism.

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

    // Set by the waiter thread the moment it has reaped the child, i.e.
    // before `pgid` can mean anything else (pid reuse). Checked before every
    // `kill_group` call below so a `StdinClosed` event that loses a race
    // against a just-finished `wait()` doesn't signal a recycled pgid.
    let reaped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Waiter thread: blocks on the child's exit. Plain std::thread, not
    // tokio — the guard must not start an async runtime.
    {
        let tx = tx.clone();
        let reaped = reaped.clone();
        std::thread::spawn(move || match child.wait() {
            Ok(status) => {
                reaped.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = tx.send(Event::ChildExited(status));
            }
            Err(e) => {
                // Near-impossible (std retries EINTR internally for us), but
                // silently dropping this would leave `rx.recv()` blocking
                // forever with a possibly-still-live, unwatched engine.
                // Synthesize a generic-failure status so the dispatch below
                // still has something to act on.
                eprintln!(
                    "nevoflux-agent --engine-guard: wait() on the child failed: {e} \
                     (treating it as exited)"
                );
                use std::os::unix::process::ExitStatusExt;
                let _ = tx.send(Event::ChildExited(std::process::ExitStatus::from_raw(
                    1 << 8,
                )));
            }
        });
    }

    // Stdin watcher: blocks reading the daemon's liveness pipe. EOF (daemon
    // closed its end, e.g. it died) means the parent is gone. A benign
    // `EINTR` is not that — `std::io::Stdin::read` does not retry it for us,
    // so treating every `Err` as "parent gone" would let a stray signal
    // SIGTERM+SIGKILL a perfectly healthy engine. Any other read error is
    // treated the same as EOF.
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
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
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
        // SIGKILL if it's still around. Each `kill_group` call is guarded by
        // `reaped`: if the waiter already reaped the child (this event lost
        // the race against a `ChildExited` that hasn't been dequeued yet),
        // `pgid` no longer names our child's group and, after pid wraparound,
        // could in principle name someone else's — skip the signal rather
        // than risk that.
        Ok(Event::StdinClosed) => {
            use nix::sys::signal::Signal;
            use std::sync::atomic::Ordering;
            if !reaped.load(Ordering::SeqCst) {
                kill_group(pgid, Signal::SIGTERM);
            }
            match rx.recv_timeout(std::time::Duration::from_secs(3)) {
                Ok(Event::ChildExited(status)) => std::process::exit(exit_code_of(status)),
                _ => {
                    if !reaped.load(Ordering::SeqCst) {
                        kill_group(pgid, Signal::SIGKILL);
                    }
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
