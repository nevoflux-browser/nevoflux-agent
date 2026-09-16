//! How the on-device inference engine (`llama-server`) is allowed to be
//! launched: the exact argv and environment, plus the API key and port a
//! launch starts with.
//!
//! `llama-server` ships with a bundled web UI, built-in agent tools, MCP
//! server attachment, and slot/props/metrics introspection endpoints -- all
//! of which must stay off for a process this daemon spawns unattended and
//! exposes only to itself over loopback (v3 brief for Task 2.5).
//! [`server_spec`] is the one place that decides the argv + env for a
//! launch; Task 2.9's engine supervisor spawns exactly what
//! [`SpawnSpec::to_command`] returns and adds nothing of its own.
//!
//! Two layers keep the launch closed even as `llama-server` grows new
//! surface between engine releases:
//! - [`FORBIDDEN_FLAGS`] is a fixed list, read off the pinned `b10909`
//!   build's own `--help` output (not assumed), of argv flags that would
//!   reopen the web UI, agent tools, MCP attachment, introspection
//!   endpoints, or let the *engine itself* reach the network. This module's
//!   own tests assert none of them ever appear in [`server_spec`]'s argv --
//!   a regression guard, not the primary defense (`server_spec` simply
//!   never emits them; nothing here parses or strips a "user-provided"
//!   flag list, because there is no such input).
//! - [`env_allowlist`] is the primary defense. Every `LLAMA_ARG_*` flag has
//!   an environment-variable equivalent (142 of them in the pinned build),
//!   so blacklisting argv alone would not be enough: a parent environment
//!   carrying `LLAMA_ARG_AGENT=1` would silently re-enable the same agent
//!   surface the argv refuses to pass. [`server_spec_with_parent_env`]
//!   starts the child's environment from nothing and copies across only
//!   the names [`env_allowlist`] lists (plus the `LLAMA_API_KEY` this
//!   module sets itself), so no `LLAMA_ARG_*`-shaped variable, `HF_TOKEN`,
//!   or anything else the daemon's own process happens to have inherited
//!   can reach the child.
//!
//! That second layer only holds if the child process's environment is
//! actually *replaced* by `SpawnSpec.env`, not merged onto whatever the
//! daemon process itself inherited -- `Command::envs(spec.env)` alone
//! compiles fine and does the wrong thing. [`SpawnSpec::to_command`] is the
//! only supported way to turn a [`SpawnSpec`] into a runnable
//! `std::process::Command`: it calls `env_clear()` internally before
//! applying `spec.env`, so that step is enforced by the API shape rather
//! than by a comment Task 2.9 has to remember to honor (Task 2.5 review
//! finding 2).
//!
//! The API key itself never appears in argv (`--api-key`/`--api-key-file`
//! are themselves in [`FORBIDDEN_FLAGS]` for exactly this reason): it
//! travels only as the `LLAMA_API_KEY` environment variable, which is the
//! one flag in the pinned build whose env-var name does NOT follow the
//! `LLAMA_ARG_*` convention (verified against `--help`, not assumed --
//! see the task brief).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::local::config::{KvCacheType, CTX_FLOOR};

/// The exact argv, environment, program, and working directory an engine
/// launch spawns with. Turn this into a runnable process only via
/// [`SpawnSpec::to_command`] -- never by hand-assembling a
/// `std::process::Command` from these public fields, which would compile
/// but silently merge `env` onto the daemon's own inherited environment
/// instead of replacing it (see the module doc comment).
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

impl SpawnSpec {
    /// The only supported way to turn this [`SpawnSpec`] into a runnable
    /// `std::process::Command`. Calls `env_clear()` before applying
    /// `self.env` -- v3 §16.1 row 2 treats this as part of the LocalOnly
    /// gate itself, not defense-in-depth, so it is enforced here rather
    /// than left to whoever spawns the process to remember. Also sets
    /// `args` and `current_dir` from `self.cwd`; the caller only needs to
    /// call `.spawn()`/`.output()`/etc.
    pub fn to_command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        cmd.env_clear();
        cmd.envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd.current_dir(&self.cwd);
        cmd
    }
}

/// Inputs [`server_spec`] turns into a [`SpawnSpec`]: what the caller (the
/// engine supervisor) has already decided elsewhere --
/// `local::hardware`'s backend/`InstallKind` selection picked `engine_dir`
/// and `platform` (the same `InstallKind.platform` string,
/// e.g. `"windows-x64"` -- see `hardware.rs`'s `platform_string`),
/// `local::memory`'s ctx/GPU-layer fit picked `ctx`/`gpu_layers`, and the
/// caller generated `port`/`api_key` itself (see [`pick_free_port`],
/// [`generate_api_key`]).
///
/// `platform` is a plain string, not `cfg!(target_os)`, deliberately (Task
/// 2.5 review finding 1): `install::sentinels_ok` already decides the
/// identical `llama-server` vs `llama-server.exe` question from this same
/// data (`kind.platform.starts_with("windows")` at `install.rs`), and a
/// compile-time `cfg!` here would be a second, independent source of truth
/// for one fact that could silently disagree with it -- as well as making
/// the Linux/macOS branches of [`server_spec_with_parent_env`] permanently
/// untestable on a Windows development machine.
pub struct ServerParams<'a> {
    pub engine_dir: &'a Path,
    pub model_path: &'a Path,
    pub platform: &'a str,
    pub port: u16,
    pub api_key: &'a str,
    pub ctx: u32,
    pub parallel: u32,
    pub kv: KvCacheType,
    pub gpu_layers: i32,
    pub flash_attn: bool,
}

/// Argv flags [`server_spec`] must never emit, and this module's tests
/// assert never appear in its output. Read off the pinned `b10909`
/// `llama-server.exe --help` itself (2026-09-16), not assumed -- see the
/// task brief for the verification note. Grouped by what each opens up:
pub const FORBIDDEN_FLAGS: &[&str] = &[
    // tools / agent (and their ALIASES -- the binary accepts both spellings)
    "--tools",
    "--tools-runtime",
    "--agent",
    "-ag",
    // MCP attachment vectors
    "--mcp-servers-config",
    "--mcp-servers-json",
    "--ui-mcp-proxy",
    "--webui-mcp-proxy",
    // web UI
    "--webui",
    "--ui",
    "--ui-config",
    "--webui-config",
    // introspection endpoints
    "--props",
    "--metrics",
    "--slot-save-path",
    // key must travel by env, never argv
    "--api-key",
    "--api-key-file",
    // anything that lets the ENGINE itself fetch or serve over the network
    "--model-url",
    "--hf-repo",
    "--docker-repo",
    "--rpc",
    "--static-path",
    "--ssl-cert-file",
    "--ssl-key-file",
];

/// Environment variable names allowed to pass from the daemon's own process
/// environment through to a spawned `llama-server`: enough for the process
/// loader, temp files, and GPU backend discovery to work, and nothing else
/// -- see the module doc comment for why this (not [`FORBIDDEN_FLAGS`]) is
/// the actual security boundary.
pub fn env_allowlist() -> &'static [&'static str] {
    &[
        "PATH",
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "HOME",
        "USERPROFILE",
        "LOCALAPPDATA",
        "CUDA_VISIBLE_DEVICES",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "VK_ICD_FILENAMES",
    ]
}

fn kv_flag_value(kv: KvCacheType) -> &'static str {
    match kv {
        KvCacheType::Q8_0 => "q8_0",
        KvCacheType::F16 => "f16",
    }
}

/// Builds the [`SpawnSpec`] for one `llama-server` launch, starting the
/// child's environment from the daemon's own real process environment
/// (`std::env::vars()`) filtered through [`env_allowlist`]. See
/// [`server_spec_with_parent_env`] for the testable version that takes an
/// explicit parent environment instead of reading the real one.
pub fn server_spec(p: &ServerParams) -> SpawnSpec {
    let parent: Vec<(String, String)> = std::env::vars().collect();
    server_spec_with_parent_env(p, &parent)
}

/// [`server_spec`], parameterized over the parent environment instead of
/// reading `std::env::vars()` -- so a test can inject a hostile parent
/// environment (e.g. `LLAMA_ARG_AGENT=1`, `HF_TOKEN=...`) and assert none
/// of it survives into the returned [`SpawnSpec::env`].
pub fn server_spec_with_parent_env(p: &ServerParams, parent_env: &[(String, String)]) -> SpawnSpec {
    // Belt: the daemon's own `LocalConfig::validated_ctx` should already
    // have rejected a sub-floor ctx long before this function is ever
    // called, so hitting this in a debug/test build means that validation
    // was skipped -- a caller bug worth panicking loudly for. Suspenders:
    // `ctx` is still clamped below, so a release build (where
    // `debug_assert!` compiles to nothing) can never actually launch the
    // engine with less than `CTX_FLOOR` tokens of context.
    debug_assert!(
        p.ctx >= CTX_FLOOR,
        "server_spec called with ctx {} below the {CTX_FLOOR} floor -- the caller should have \
         validated via LocalConfig::validated_ctx before reaching here",
        p.ctx
    );
    let ctx = p.ctx.max(CTX_FLOOR);

    let ngl = if p.gpu_layers == -1 { 99 } else { p.gpu_layers };
    let kv_value = kv_flag_value(p.kv);
    let fa_value = if p.flash_attn { "on" } else { "off" };

    let args = vec![
        "-m".to_string(),
        p.model_path.display().to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        p.port.to_string(),
        "--no-webui".to_string(),
        "--no-slots".to_string(),
        "--cors-origins".to_string(),
        "https://nevoflux.invalid".to_string(),
        "--no-cors-credentials".to_string(),
        "-c".to_string(),
        ctx.to_string(),
        "--parallel".to_string(),
        p.parallel.to_string(),
        "--kv-unified".to_string(),
        "-ngl".to_string(),
        ngl.to_string(),
        "--jinja".to_string(),
        "--cache-type-k".to_string(),
        kv_value.to_string(),
        "--cache-type-v".to_string(),
        kv_value.to_string(),
        "-fa".to_string(),
        fa_value.to_string(),
    ];

    // A `BTreeMap` (not a `Vec`) while building `env`, so setting
    // `LLAMA_API_KEY`/`LD_LIBRARY_PATH`/`DYLD_LIBRARY_PATH` below can never
    // produce a duplicate-key entry alongside an allowlisted parent value
    // of the same name -- it overwrites in place instead.
    let allow: BTreeSet<&str> = env_allowlist().iter().copied().collect();
    let mut env: BTreeMap<String, String> = parent_env
        .iter()
        .filter(|(k, _)| allow.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.insert("LLAMA_API_KEY".to_string(), p.api_key.to_string());
    if p.platform.starts_with("linux") {
        // The pinned Linux archives' shared libraries (libggml*.so,
        // libllama.so -- see `install::SENTINEL_TABLE`) live in
        // `engine_dir` itself, not a system library path; the loader will
        // not find them without this. Windows resolves DLLs via the
        // working directory (`cwd` below) instead.
        env.insert(
            "LD_LIBRARY_PATH".to_string(),
            p.engine_dir.display().to_string(),
        );
    } else if p.platform.starts_with("macos") {
        // Task 2.5 review finding 9: passing DYLD_LIBRARY_PATH through the
        // allowlist verbatim is the same shared-library injection shape
        // the Linux branch above already closes for LD_LIBRARY_PATH --
        // force it to engine_dir here too rather than leaving an
        // asymmetric gap. macOS's archives are self-contained `.dylib`s
        // the dynamic linker would already find via same-directory search,
        // so this closes an injection vector rather than fixing a
        // functional gap.
        env.insert(
            "DYLD_LIBRARY_PATH".to_string(),
            p.engine_dir.display().to_string(),
        );
    }

    let binary = if p.platform.starts_with("windows") {
        "llama-server.exe"
    } else {
        "llama-server"
    };

    SpawnSpec {
        program: p.engine_dir.join(binary),
        args,
        env: env.into_iter().collect(),
        cwd: p.engine_dir.to_path_buf(),
    }
}

/// Generates a fresh 32-byte (64 hex char) API key for one engine launch,
/// via `rand::thread_rng` -- the same construction
/// `crate::llm_gateway::generate_random_token` already uses for the
/// llm-gateway's own bearer token.
pub fn generate_api_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reserves a free loopback TCP port by binding `127.0.0.1:0` (letting the
/// OS assign one), reading it back, and dropping the listener. Inherently
/// TOCTOU -- nothing stops another process from grabbing the same port
/// between this returning and the engine supervisor actually launching
/// `llama-server` on it -- so the caller must be prepared for the launch
/// itself to fail with the port already in use and retry, not treat this
/// as a hard reservation.
pub fn pick_free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_params<'a>(
        engine_dir: &'a Path,
        model_path: &'a Path,
        api_key: &'a str,
        platform: &'a str,
    ) -> ServerParams<'a> {
        ServerParams {
            engine_dir,
            model_path,
            platform,
            port: 45123,
            api_key,
            ctx: 32768,
            parallel: 2,
            kv: KvCacheType::Q8_0,
            gpu_layers: -1,
            flash_attn: true,
        }
    }

    // --- FORBIDDEN_FLAGS -------------------------------------------------

    #[test]
    fn no_forbidden_flag_appears_in_argv_as_a_whole_element_or_a_flag_position_substring() {
        let engine_dir = PathBuf::from("/engine");
        // Deliberately a path containing the literal "-ag" (this product's
        // own directory is "nevoflux-agent", and "-ag" is FORBIDDEN_FLAGS'
        // alias for --agent) -- proves the substring check below is
        // restricted to flag-position elements and does not false-positive
        // on a realistic value (Task 2.5 review finding 4).
        let model_path = PathBuf::from("/opt/nevoflux-agent/models/m.gguf");
        let p = sample_params(&engine_dir, &model_path, "secret", "windows-x64");
        let spec = server_spec(&p);
        for forbidden in FORBIDDEN_FLAGS {
            for arg in &spec.args {
                assert_ne!(arg, forbidden, "argv contains forbidden flag {forbidden}");
                if arg.starts_with('-') {
                    assert!(
                        !arg.contains(forbidden),
                        "argv element {arg:?} contains forbidden flag {forbidden}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_api_key_never_appears_in_argv_only_in_env() {
        // v3 §16: the key must travel by env, never argv.
        let engine_dir = PathBuf::from("/engine");
        let model_path = PathBuf::from("/models/m.gguf");
        let p = sample_params(&engine_dir, &model_path, "super-secret-key", "windows-x64");
        let spec = server_spec(&p);
        assert!(spec.args.iter().all(|a| !a.contains("super-secret-key")));
        assert!(spec
            .env
            .iter()
            .any(|(k, v)| k == "LLAMA_API_KEY" && v == "super-secret-key"));
    }

    // --- env: prefix allowlist, not a named-variable blacklist ------------

    #[test]
    fn env_is_a_prefix_allowlist_that_survives_a_hostile_parent_environment() {
        let engine_dir = PathBuf::from("/engine");
        let model_path = PathBuf::from("/models/m.gguf");
        let p = sample_params(&engine_dir, &model_path, "secret", "windows-x64");
        let parent = vec![
            ("LLAMA_ARG_AGENT".to_string(), "1".to_string()),
            ("LLAMA_ARG_TOOLS".to_string(), "all".to_string()),
            ("LLAMA_ARG_MCP_SERVERS_JSON".to_string(), "{}".to_string()),
            ("LLAMA_ARG_HOST".to_string(), "0.0.0.0".to_string()),
            ("LLAMA_ARG_MODEL_URL".to_string(), "http://evil".to_string()),
            ("HF_TOKEN".to_string(), "secret-token".to_string()),
            // An allowlisted variable, to confirm the filter is a positive
            // allowlist and not something that (accidentally) drops
            // everything.
            ("PATH".to_string(), "/usr/bin".to_string()),
        ];
        let spec = server_spec_with_parent_env(&p, &parent);

        for (k, _) in &spec.env {
            assert!(
                !k.starts_with("LLAMA_") || k == "LLAMA_API_KEY",
                "env leaked a LLAMA_-prefixed variable from the parent: {k}"
            );
        }
        assert!(
            spec.env.iter().all(|(k, _)| k != "HF_TOKEN"),
            "env leaked HF_TOKEN from the parent"
        );
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "LLAMA_API_KEY" && v == "secret"),
            "our own LLAMA_API_KEY must still be set"
        );
        assert!(
            spec.env.iter().any(|(k, v)| k == "PATH" && v == "/usr/bin"),
            "an allowlisted variable from the parent must survive"
        );
    }

    #[test]
    fn env_allowlist_matches_the_documented_set() {
        assert_eq!(
            env_allowlist(),
            &[
                "PATH",
                "SYSTEMROOT",
                "WINDIR",
                "TEMP",
                "TMP",
                "HOME",
                "USERPROFILE",
                "LOCALAPPDATA",
                "CUDA_VISIBLE_DEVICES",
                "LD_LIBRARY_PATH",
                "DYLD_LIBRARY_PATH",
                "VK_ICD_FILENAMES",
            ]
        );
    }

    // --- ctx floor: debug_assert + clamp -----------------------------------

    #[test]
    fn ctx_at_the_floor_passes_through_unclamped() {
        let engine_dir = PathBuf::from("/engine");
        let model_path = PathBuf::from("/models/m.gguf");
        let mut p = sample_params(&engine_dir, &model_path, "secret", "windows-x64");
        p.ctx = CTX_FLOOR;
        let spec = server_spec(&p);
        let idx = spec.args.iter().position(|a| a == "-c").unwrap();
        assert_eq!(spec.args[idx + 1], CTX_FLOOR.to_string());
    }

    #[test]
    #[should_panic(expected = "below the")]
    fn ctx_below_the_floor_debug_asserts() {
        // debug_assertions are on for this crate's dev/test profiles (no
        // [profile.dev]/[profile.test] override -- see
        // install::tests::kind_suffix_round_trips... for the same fact
        // noted independently), so this reaches the assert rather than the
        // release-only clamp.
        let engine_dir = PathBuf::from("/engine");
        let model_path = PathBuf::from("/models/m.gguf");
        let mut p = sample_params(&engine_dir, &model_path, "secret", "windows-x64");
        p.ctx = CTX_FLOOR - 1;
        let _ = server_spec(&p);
    }

    // --- -ngl: -1 means "as many as fit", spelled 99 for the engine -------

    #[test]
    fn gpu_layers_negative_one_becomes_99() {
        let engine_dir = PathBuf::from("/engine");
        let model_path = PathBuf::from("/models/m.gguf");
        let mut p = sample_params(&engine_dir, &model_path, "secret", "windows-x64");
        p.gpu_layers = -1;
        let spec = server_spec(&p);
        let idx = spec.args.iter().position(|a| a == "-ngl").unwrap();
        assert_eq!(spec.args[idx + 1], "99");
    }

    #[test]
    fn gpu_layers_a_fixed_count_passes_through() {
        let engine_dir = PathBuf::from("/engine");
        let model_path = PathBuf::from("/models/m.gguf");
        let mut p = sample_params(&engine_dir, &model_path, "secret", "windows-x64");
        p.gpu_layers = 20;
        let spec = server_spec(&p);
        let idx = spec.args.iter().position(|a| a == "-ngl").unwrap();
        assert_eq!(spec.args[idx + 1], "20");
    }

    // --- kv cache type / flash attention -----------------------------------

    #[test]
    fn kv_cache_type_maps_to_the_expected_flag_values() {
        assert_eq!(kv_flag_value(KvCacheType::Q8_0), "q8_0");
        assert_eq!(kv_flag_value(KvCacheType::F16), "f16");
    }

    #[test]
    fn flash_attn_maps_to_on_or_off() {
        let engine_dir = PathBuf::from("/engine");
        let model_path = PathBuf::from("/models/m.gguf");
        let mut p = sample_params(&engine_dir, &model_path, "secret", "windows-x64");

        p.flash_attn = true;
        let spec = server_spec(&p);
        let idx = spec.args.iter().position(|a| a == "-fa").unwrap();
        assert_eq!(spec.args[idx + 1], "on");

        p.flash_attn = false;
        let spec = server_spec(&p);
        let idx = spec.args.iter().position(|a| a == "-fa").unwrap();
        assert_eq!(spec.args[idx + 1], "off");
    }

    // --- program / cwd / platform-driven branching (Task 2.5 review 1, 8) --

    #[test]
    fn program_and_cwd_are_rooted_at_engine_dir_per_platform() {
        let engine_dir = PathBuf::from("/engine");
        let model_path = PathBuf::from("/models/m.gguf");
        for (platform, expected_binary) in [
            ("windows-x64", "llama-server.exe"),
            ("windows-arm64", "llama-server.exe"),
            ("linux-x64", "llama-server"),
            ("linux-arm64", "llama-server"),
            ("macos-x64", "llama-server"),
            ("macos-arm64", "llama-server"),
        ] {
            let p = sample_params(&engine_dir, &model_path, "secret", platform);
            let spec = server_spec(&p);
            assert_eq!(
                spec.program,
                engine_dir.join(expected_binary),
                "platform {platform}"
            );
            assert_eq!(spec.cwd, engine_dir, "platform {platform}");
        }
    }

    #[test]
    fn linux_forces_ld_library_path_to_engine_dir() {
        let engine_dir = PathBuf::from("/opt/engine");
        let model_path = PathBuf::from("/models/m.gguf");
        let p = sample_params(&engine_dir, &model_path, "secret", "linux-x64");
        // A hostile/stale parent value must be overridden, not merely
        // supplemented.
        let parent = vec![(
            "LD_LIBRARY_PATH".to_string(),
            "/some/other/place".to_string(),
        )];
        let spec = server_spec_with_parent_env(&p, &parent);
        assert!(spec
            .env
            .iter()
            .any(|(k, v)| k == "LD_LIBRARY_PATH" && v == "/opt/engine"));
        assert!(spec.env.iter().all(|(k, _)| k != "DYLD_LIBRARY_PATH"));
    }

    #[test]
    fn macos_forces_dyld_library_path_to_engine_dir() {
        let engine_dir = PathBuf::from("/opt/engine");
        let model_path = PathBuf::from("/models/m.gguf");
        let p = sample_params(&engine_dir, &model_path, "secret", "macos-arm64");
        let parent = vec![(
            "DYLD_LIBRARY_PATH".to_string(),
            "/some/hostile/place".to_string(),
        )];
        let spec = server_spec_with_parent_env(&p, &parent);
        assert!(spec
            .env
            .iter()
            .any(|(k, v)| k == "DYLD_LIBRARY_PATH" && v == "/opt/engine"));
        assert!(spec.env.iter().all(|(k, _)| k != "LD_LIBRARY_PATH"));
    }

    #[test]
    fn windows_does_not_force_either_library_path_variable() {
        // Uses an explicit empty parent env (not `server_spec`'s real
        // `std::env::vars()`) so this doesn't depend on whether the host
        // running the test happens to have LD_LIBRARY_PATH set (e.g. a
        // Git-Bash/MSYS shell) -- this asserts windows-x64 never FORCES
        // either var, not that a passthrough value can never appear.
        let engine_dir = PathBuf::from("C:\\engine");
        let model_path = PathBuf::from("C:\\models\\m.gguf");
        let p = sample_params(&engine_dir, &model_path, "secret", "windows-x64");
        let spec = server_spec_with_parent_env(&p, &[]);
        assert!(spec.env.iter().all(|(k, _)| k != "LD_LIBRARY_PATH"));
        assert!(spec.env.iter().all(|(k, _)| k != "DYLD_LIBRARY_PATH"));
    }

    // --- generate_api_key / pick_free_port ----------------------------------

    #[test]
    fn generate_api_key_is_32_bytes_of_hex() {
        let k = generate_api_key();
        assert_eq!(k.len(), 64, "32 bytes -> 64 hex chars");
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_api_key_is_distinct_between_calls() {
        assert_ne!(generate_api_key(), generate_api_key());
    }

    #[test]
    fn pick_free_port_returns_a_nonzero_port_and_a_second_call_also_succeeds() {
        // The function's own doc comment states this is NOT a reservation
        // (inherently TOCTOU) -- asserting that the exact same port can be
        // rebound after drop would assert the opposite of the documented
        // contract, and would itself be racy (Task 2.5 review finding 7).
        // What is safe to assert: a nonzero port comes back, and calling
        // it again also succeeds (not necessarily with the same port).
        let port = pick_free_port().expect("bind 127.0.0.1:0 should succeed in a test sandbox");
        assert_ne!(port, 0);
        let second = pick_free_port().expect("a second call should also succeed");
        assert_ne!(second, 0);
    }

    // --- SpawnSpec::to_command (Task 2.5 review finding 2) ------------------

    #[cfg(windows)]
    #[test]
    fn to_command_clears_the_parent_process_environment() {
        // A real subprocess spawn (not just inspecting `Command::get_envs`)
        // is the only way to actually prove `env_clear()` ran: `get_envs`
        // only reflects vars explicitly set on the builder, not whether
        // inherited ones were dropped. Guarded because it mutates a
        // real process-wide env var, shared with every other test in this
        // crate that does the same (see `llm_gateway::tests::ENV_MUTEX`).
        let _guard = crate::llm_gateway::tests::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NEVOFLUX_HOSTILE_TEST_VAR", "leak-me-if-you-can");
        struct RemoveOnDrop;
        impl Drop for RemoveOnDrop {
            fn drop(&mut self) {
                std::env::remove_var("NEVOFLUX_HOSTILE_TEST_VAR");
            }
        }
        let _cleanup = RemoveOnDrop;

        // cmd.exe needs SystemRoot to start at all with a cleared
        // environment; resolved from the real environment rather than
        // hardcoded so this doesn't depend on the default install path.
        let comspec = std::env::var("ComSpec")
            .unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".to_string());
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());

        let spec = SpawnSpec {
            program: PathBuf::from(comspec),
            args: vec!["/C".to_string(), "set".to_string()],
            env: vec![
                ("SystemRoot".to_string(), system_root),
                ("LLAMA_API_KEY".to_string(), "should-survive".to_string()),
            ],
            cwd: std::env::temp_dir(),
        };

        let output = spec
            .to_command()
            .output()
            .expect("cmd.exe should spawn even with a cleared environment");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            !text.contains("NEVOFLUX_HOSTILE_TEST_VAR"),
            "the child inherited a variable from the parent process despite env_clear()"
        );
        assert!(
            text.contains("LLAMA_API_KEY=should-survive"),
            "an explicitly-set env var must still reach the child"
        );
    }

    #[cfg(unix)]
    #[test]
    fn to_command_clears_the_parent_process_environment() {
        let _guard = crate::llm_gateway::tests::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NEVOFLUX_HOSTILE_TEST_VAR", "leak-me-if-you-can");
        struct RemoveOnDrop;
        impl Drop for RemoveOnDrop {
            fn drop(&mut self) {
                std::env::remove_var("NEVOFLUX_HOSTILE_TEST_VAR");
            }
        }
        let _cleanup = RemoveOnDrop;

        let spec = SpawnSpec {
            program: PathBuf::from("/usr/bin/env"),
            args: vec![],
            env: vec![("LLAMA_API_KEY".to_string(), "should-survive".to_string())],
            cwd: std::env::temp_dir(),
        };

        let output = spec
            .to_command()
            .output()
            .expect("/usr/bin/env should spawn even with a cleared environment");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            !text.contains("NEVOFLUX_HOSTILE_TEST_VAR"),
            "the child inherited a variable from the parent process despite env_clear()"
        );
        assert!(
            text.contains("LLAMA_API_KEY=should-survive"),
            "an explicitly-set env var must still reach the child"
        );
    }

    #[test]
    fn to_command_sets_argv_and_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let spec = SpawnSpec {
            program: PathBuf::from("llama-server"),
            args: vec!["--port".to_string(), "1234".to_string()],
            env: vec![],
            cwd: dir.path().to_path_buf(),
        };
        let cmd = spec.to_command();
        assert_eq!(cmd.get_program(), "llama-server");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), vec!["--port", "1234"]);
        assert_eq!(cmd.get_current_dir(), Some(dir.path()));
    }
}
