//! On-device inference, end to end, against a real engine and a real model.
//!
//! `#[ignore]` throughout: these download an engine and a multi-gigabyte model
//! and then run it. They belong to a person deciding to run them, not to
//! `cargo test`.
//!
//! ```text
//! # serve the staged assets locally, then:
//! NEVOFLUX_ENGINE_MIRROR_BASE=http://127.0.0.1:18999 \
//!   cargo test -p nevoflux-daemon --test local_e2e -- --ignored --nocapture
//! ```
//!
//! ## What has and has not been executed
//!
//! **Authored on Windows; NEVER EXECUTED, on any platform.** Compiled only.
//! Read that literally: passing `--no-run` proves these type-check against the
//! real APIs and nothing more. In particular the `#[cfg(unix)]` tests below are
//! stripped *before type-checking* on Windows, so they have not even been
//! compiled here — a Linux build could fail outright rather than misbehave.
//!
//! That distinction is the whole reason this file is blunt about it. Task 2.9's
//! first attempt never spawned the engine guard at all, and nothing failed,
//! because the guard's own tests are `#[cfg(unix)]` and compiled out on the
//! development machine. "The test didn't run" and "the code was never called"
//! read identically from Windows and are very different defects.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use nevoflux_daemon::config::AgentConfig;
use nevoflux_daemon::local::state::LocalState;
use nevoflux_daemon::local::{endpoint, engine, rpc};
use nevoflux_daemon::wasm::llm::{execute_llm_chat, LlmChatRequest, LlmMessage};
use nevoflux_daemon::DaemonError;
use nevoflux_llm::ProviderType;
use serial_test::serial;

/// The model these tests install. The 4B is the smallest that still exercises
/// the same install/launch/tool-call path as the shipped default.
const MODEL: &str = "qwen3-4b-instruct-2507";
const QUANT: &str = "Q4_K_M";

/// How long to wait for an install to reach `Ready`. Generous because it
/// includes a real download; a failure here should read as "it never got
/// there", not as a flaky timeout.
const READY_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// The RPC handlers return an envelope; the interesting part is under `data`.
///
/// Written tolerantly because the envelope's exact nesting is an internal
/// detail of `rpc.rs` (its own tests reach it through a `#[cfg(test)]` helper
/// this crate cannot see). A shape change should make these tests fail on the
/// assertion that matters, not on a missing key.
fn data(v: &serde_json::Value) -> &serde_json::Value {
    for path in [["payload", "data"].as_slice(), ["data"].as_slice()] {
        let mut cur = v;
        let mut ok = true;
        for key in path {
            match cur.get(*key) {
                Some(next) => cur = next,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            return cur;
        }
    }
    v
}

fn err_code(v: &serde_json::Value) -> Option<&str> {
    for path in [
        ["payload", "error", "code"].as_slice(),
        ["error", "code"].as_slice(),
    ] {
        let mut cur = v;
        let mut ok = true;
        for key in path {
            match cur.get(*key) {
                Some(next) => cur = next,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            return cur.as_str();
        }
    }
    None
}

fn shared_config() -> Arc<RwLock<Arc<AgentConfig>>> {
    Arc::new(RwLock::new(Arc::new(AgentConfig::default())))
}

fn req(id: &str) -> serde_json::Value {
    serde_json::json!({ "request_id": id })
}

/// Point the engine root and model dir at a temp tree, so a run never touches
/// the developer's real install.
struct TempCache {
    _dir: tempfile::TempDir,
}

impl TempCache {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        std::env::set_var("NEVOFLUX_LOCAL_CACHE_DIR", dir.path());
        Self { _dir: dir }
    }
}

impl Drop for TempCache {
    fn drop(&mut self) {
        std::env::remove_var("NEVOFLUX_LOCAL_CACHE_DIR");
    }
}

async fn wait_for_ready() -> LocalState {
    let started = Instant::now();
    loop {
        let state = engine::supervisor().state();
        match &state {
            LocalState::Ready { .. } => return state,
            LocalState::Failed { error, .. } => panic!("install failed: {error:?}"),
            _ => {}
        }
        assert!(
            started.elapsed() < READY_TIMEOUT,
            "engine never reached Ready (last state: {state:?})"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn user(text: &str) -> LlmChatRequest {
    LlmChatRequest {
        messages: vec![LlmMessage {
            role: "user".into(),
            content: text.into(),
            tool_calls: None,
            tool_call_id: None,
            attachments: Vec::new(),
            reasoning: None,
        }],
        system: None,
        temperature: None,
        max_tokens: Some(64),
        tools: None,
    }
}

/// probe -> plan -> install -> Ready -> set_default, then the invariant the
/// whole latch exists for: on-device answers, and the network does not.
#[tokio::test]
#[ignore = "downloads an engine and a model, then runs it"]
#[serial]
async fn on_device_answers_and_the_network_is_refused() {
    let _cache = TempCache::new();
    let cfg = shared_config();

    let probe = rpc::handle_probe(&req("p")).await;
    assert!(
        data(&probe).get("os").is_some(),
        "probe should report this machine: {probe}"
    );

    let plan = rpc::handle_plan(&serde_json::json!({
        "request_id": "pl", "model": MODEL, "quant": QUANT,
    }))
    .await;
    let plan_data = data(&plan);
    assert!(
        plan_data.get("engine_bytes").is_some() && plan_data.get("model_bytes").is_some(),
        "plan should price both halves: {plan}"
    );

    let install = rpc::handle_install(
        &serde_json::json!({
            "request_id": "i", "model": MODEL, "quant": QUANT, "backend": "auto",
        }),
        &cfg,
    )
    .await;
    assert_eq!(
        data(&install).get("started").and_then(|v| v.as_bool()),
        Some(true),
        "install should start and report progress on events: {install}"
    );

    let ready = wait_for_ready().await;
    let LocalState::Ready { ctx, .. } = &ready else {
        unreachable!("wait_for_ready only returns Ready")
    };
    assert!(*ctx >= 16_384, "ctx floor is 16K, got {ctx}");
    assert!(
        endpoint::current().is_some(),
        "a Ready engine must publish an endpoint"
    );

    let on =
        rpc::handle_set_default(&serde_json::json!({"request_id": "d", "on": true}), &cfg).await;
    assert!(err_code(&on).is_none(), "set_default on failed: {on}");

    // The engine answers.
    let local = execute_llm_chat(
        ProviderType::Local,
        "",
        MODEL,
        user("Reply with the single word: ok"),
        None,
    )
    .await
    .expect("the local engine should answer");
    assert!(
        !local.content.is_empty(),
        "local reply was empty (finish_reason: {})",
        local.finish_reason
    );

    // And the network does not. This is the latch's entire purpose.
    let cloud = execute_llm_chat(
        ProviderType::Anthropic,
        "sk-not-a-real-key",
        "claude-sonnet-4",
        user("hello"),
        None,
    )
    .await;
    match cloud {
        Err(DaemonError::PermissionDenied(_)) => {}
        Err(other) => panic!("expected PermissionDenied while latched, got {other:?}"),
        Ok(_) => panic!("a cloud provider answered while the latch was on"),
    }

    // Summarization must stay on-device too — it used to fall back to cloud.
    let (provider, _key) =
        nevoflux_daemon::context::get_summarization_provider(&AgentConfig::default(), MODEL)
            .expect("summarization provider resolves while latched");
    assert_eq!(provider, ProviderType::Local);

    // Releasing requires naming a configured provider (on:false alone is
    // rejected, and an unconfigured target is rejected too).
    let missing =
        rpc::handle_set_default(&serde_json::json!({"request_id": "r1", "on": false}), &cfg).await;
    assert_eq!(err_code(&missing), Some("missing_provider"));

    engine::supervisor().stop().await;
    assert!(
        endpoint::current().is_none(),
        "stop() must retract the endpoint"
    );
}

/// The guard, and the orphan it exists to prevent.
///
/// `stop()` proves nothing about this: it terminates the child directly, so it
/// passes whether or not the guard exists. The guard only matters when the
/// daemon dies *without* running `stop()` — which is exactly the path nothing
/// has ever driven.
#[cfg(unix)]
#[tokio::test]
#[ignore = "downloads an engine and a model, then kills processes"]
#[serial]
async fn the_engine_is_a_child_of_the_guard_and_dies_with_the_daemon() {
    let _cache = TempCache::new();
    let cfg = shared_config();

    rpc::handle_install(
        &serde_json::json!({
            "request_id": "i", "model": MODEL, "quant": QUANT, "backend": "auto",
        }),
        &cfg,
    )
    .await;
    wait_for_ready().await;

    // (a) The spawned child must be the guard, with llama-server as *its*
    //     child — not a direct llama-server.
    let pid = std::process::id();
    let children = std::process::Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .expect("pgrep");
    let child_pids = String::from_utf8_lossy(&children.stdout);
    let mut saw_guard = false;
    for line in child_pids.lines() {
        let cmd = std::fs::read_to_string(format!("/proc/{}/cmdline", line.trim()))
            .unwrap_or_default()
            .replace('\0', " ");
        if cmd.contains("--engine-guard") {
            saw_guard = true;
            let grandchildren = std::process::Command::new("pgrep")
                .args(["-P", line.trim()])
                .output()
                .expect("pgrep");
            assert!(
                !grandchildren.stdout.is_empty(),
                "the guard has no child; llama-server should be under it"
            );
        }
        assert!(
            !cmd.contains("llama-server"),
            "llama-server is a DIRECT child — the guard was bypassed, which is \
             how the orphan mechanism silently went missing before"
        );
    }
    assert!(saw_guard, "no --engine-guard child was spawned");

    engine::supervisor().stop().await;
}

/// The guard pipe must be `CLOEXEC` (v3 §16.2 mandates the assertion).
///
/// Rust's std sets `O_CLOEXEC` on pipes it creates, so this almost certainly
/// holds — but it is *defaulted, not asserted*, and the spec asked for the
/// assertion precisely so a future change or a different spawn path cannot
/// silently widen the hole. It also underpins the narrow-reachability argument
/// used to accept the known reap gap: that argument assumes the pipe's write
/// end cannot leak into an unrelated process.
#[cfg(unix)]
#[test]
#[ignore = "needs a running engine"]
fn the_guard_pipe_is_cloexec() {
    use std::os::fd::AsRawFd;

    let (reader, _writer) = std::io::pipe().expect("pipe");
    let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0, "F_GETFD failed");
    assert!(
        flags & libc::FD_CLOEXEC != 0,
        "the pipe std handed us is not CLOEXEC; the guard's write end could \
         leak into an unrelated process"
    );
}
