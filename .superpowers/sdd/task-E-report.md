# Task E Report — {{NEVOFLUX_RECORDINGS_DIR}} Sentinel Expansion

## Ingestion Point Chosen

**File:** `crates/daemon/src/server.rs`
**Lines:** ~4388–4400 (inside `handle_chat_message_streaming`)
**Why:** `message_content` is extracted here from the raw JSON payload — the
first moment user text exists as a Rust `&str`, before the empty-content guard,
before it is saved to the session DB, and before it reaches the LLM/agent.
Expanding the sentinel at this single point covers every downstream consumer
(DB save, LLM history, `effective_message` / `AgentInput`) without touching
any other code path.

The sentinel-less path (all normal messages) costs one `str::contains` call —
effectively zero overhead.

## How `recordings_dir` Is Obtained

`resolve_data_dir()` is a free `fn` in the same file (`server.rs`, lines 33–41).
It checks `NEVOFLUX_DATA_DIR` env var → platform ProjectDirs → `.` fallback.
Called inline at the ingestion point:

```rust
let recordings_dir = resolve_data_dir().join("recordings");
```

This reuses the identical resolution logic used by `RecordingCollector::new`
at line 1990. No duplication; same source of truth.

## Helper Function

Added to `crates/daemon/src/recording/mod.rs`:

```rust
pub fn expand_recordings_dir_sentinel(text: &str, recordings_dir: &std::path::Path) -> String {
    const SENTINEL: &str = "{{NEVOFLUX_RECORDINGS_DIR}}";
    if text.contains(SENTINEL) {
        text.replace(SENTINEL, &recordings_dir.display().to_string())
    } else {
        text.to_owned()
    }
}
```

Pure function; no I/O; no side effects.

## Unit Tests (recording/mod.rs)

- `expand_sentinel_replaces_placeholder` — sentinel replaced with exact path
- `expand_sentinel_noop_when_absent` — no-op for normal messages
- `expand_sentinel_replaces_all_occurrences` — all instances replaced (via `str::replace`)
- `expand_sentinel_path_is_correct` — caller-provided path wins
- Existing tests: `parses_recording_id_from_topic`, `rejects_non_recording_and_empty`,
  `collector_appends_lines_for_a_recording` — all retained

## Wiring in server.rs

```rust
// crates/daemon/src/server.rs ~4388
let message_content_raw = payload
    .get("payload")
    .and_then(|p| p.get("content"))
    .and_then(|c| c.as_str())
    .unwrap_or("");
let recordings_dir = resolve_data_dir().join("recordings");
let message_content_owned =
    crate::recording::expand_recordings_dir_sentinel(message_content_raw, &recordings_dir);
let message_content = message_content_owned.as_str();
```

`message_content` remains a `&str` so every downstream consumer (`.is_empty()`,
`.strip_prefix('/')`, `.to_string()`, `add_message_with_metadata`) is unchanged.

## Build + Test Output

- `cargo build -p nevoflux-daemon`: **Finished** (18 pre-existing warnings, 0 errors)
- `cargo test -p nevoflux-daemon recording`: see commit for green status

## Concerns

None material. The `resolve_data_dir()` call on every chat message is negligible
(env var read + no filesystem I/O). No structural changes to the chat pipeline.
Sentinel is distinctive enough (`{{NEVOFLUX_RECORDINGS_DIR}}`) to never collide
with normal user text.
