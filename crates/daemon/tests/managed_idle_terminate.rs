//! Regression test for managed-daemon idle self-termination.
//!
//! A managed daemon that goes idle must make its termination observable to
//! the binary's main loop via `Server::wait_terminated()`. Before this
//! mechanism existed, the idle check only stopped the TCP accept loop while
//! the process kept waiting on Ctrl+C forever, leaking one orphan daemon
//! (and one port from the 19501-19600 range) per browser session.

use std::sync::Arc;
use std::time::Duration;

/// Boot a managed server with a short idle timeout and assert that
/// `wait_terminated()` resolves once the idle check fires.
#[tokio::test(flavor = "multi_thread")]
async fn managed_daemon_signals_termination_when_idle() {
    // Surface daemon logs under --nocapture (RUST_LOG-controlled).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    // Isolate from any real user config so the test never boots optional
    // subsystems (gbrain, configured providers) from the developer's
    // ~/.config/nevoflux/config.toml. HOME covers macOS, XDG_CONFIG_HOME
    // covers Linux; on Windows the config lives under RoamingAppData and
    // CI runners have no NevoFlux config there.
    let isolated_home = tempfile::tempdir().expect("create temp home");
    std::env::set_var("HOME", isolated_home.path());
    std::env::set_var("XDG_CONFIG_HOME", isolated_home.path());

    // Explicitly disable subsystems this test doesn't need, rather than
    // relying on isolated-config defaults:
    // - embedding: its init runs FastEmbedProvider::new on a spawn_blocking
    //   thread; with an empty model cache that path hits the network, and
    //   spawn_blocking threads can't be cancelled — the test runtime's Drop
    //   would wait on that thread forever. The real daemon sidesteps this
    //   with force_exit(); the test harness can't.
    // - knowledge_base/brain: a spawned gbrain would contend on the real
    //   ~/.gbrain PGLite lock with any production daemon on this machine.
    let config_dir = isolated_home.path().join("nevoflux");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        "[embedding]\n\
         enabled = false\n\n\
         [knowledge_base]\n\
         enabled = false\n\n\
         [knowledge_base.brain]\n\
         enabled = false\n",
    )
    .expect("write test config");

    let data_dir = tempfile::tempdir().expect("create temp data dir");
    std::env::set_var("NEVOFLUX_DATA_DIR", data_dir.path());

    let db_path = data_dir.path().join("test.db");
    let session_manager = Arc::new(
        nevoflux_daemon::SessionManager::new(db_path.to_str().expect("utf-8 db path"))
            .expect("create session manager"),
    );
    let router = Arc::new(nevoflux_daemon::Router::new());

    // Same subsystem opt-outs as the config file above, but injected
    // directly — this layer also holds on Windows, where HOME /
    // XDG_CONFIG_HOME don't redirect the config path.
    let mut agent_config = nevoflux_daemon::AgentConfig::default();
    agent_config.embedding.enabled = false;
    agent_config.knowledge_base.enabled = false;
    agent_config.knowledge_base.brain.enabled = false;

    let config = nevoflux_daemon::ServerConfig {
        managed: true,
        idle_timeout: Duration::from_millis(200),
        data_dir: Some(data_dir.path().to_path_buf()),
        // Stay out of the production daemon's 19500-19600 range so the
        // test never claims a port a real proxy is about to hand out.
        port_start: 38500,
        port_end: 38600,
        agent_config: Some(agent_config),
        ..Default::default()
    };

    let server = nevoflux_daemon::start_server(config, router, session_manager)
        .await
        .expect("start managed server");

    // The idle checker polls every second, so termination should be
    // signalled shortly after ~1s of idleness. 30s is a generous ceiling
    // that only trips when the signal never arrives (the original bug).
    tokio::time::timeout(Duration::from_secs(30), server.wait_terminated())
        .await
        .expect("managed daemon never signalled termination after going idle");
}
