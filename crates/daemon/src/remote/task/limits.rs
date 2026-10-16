//! The numbers delegation mode runs on (M3 contract §11), in one place so
//! that changing one is one edit.

use std::time::Duration;

/// Tasks one pairing may have running at once (decision C6).
pub const RUNNING_PER_PAIRING: usize = 1;
/// Successful starts one pairing may make in any trailing window.
pub const STARTS_PER_WINDOW: usize = 20;
/// That window, in seconds.
pub const START_WINDOW_SECS: i64 = 3600;
/// How long a question in a delegated task waits before it is refused
/// (registry deadline and plan wait alike, §3.8).
pub const DELEGATED_HOLD: Duration = Duration::from_secs(1800);
/// How long a finished task can still be waited on.
pub const TERMINAL_RETENTION_SECS: i64 = 3600;
pub const WAIT_DEFAULT_SECS: u64 = 30;
pub const WAIT_MAX_SECS: u64 = 60;
pub const PROMPT_MAX_CHARS: usize = 8000;
pub const ANSWER_MAX_CHARS: usize = 4000;
pub const REPLY_MAX_CHARS: usize = 32_000;
/// Step events kept per task; older ones are dropped (`steps_truncated`).
pub const STEPS_MAX: usize = 500;
pub const APPROVAL_SUMMARY_MAX_CHARS: usize = 200;
pub const STATUS_SUMMARY_MAX_CHARS: usize = 80;
pub const SCREENSHOT_QUALITY: u8 = 60;
pub const SCREENSHOT_RETRY_QUALITY: u8 = 40;
pub const SCREENSHOT_MAX_WIDTH: u32 = 1280;
pub const SCREENSHOT_MAX_HEIGHT: u32 = 2400;
pub const SCREENSHOT_MAX_BYTES: u64 = 307_200;
/// The extension's own timeout for the screenshot request.
pub const SCREENSHOT_REQUEST_MS: u64 = 30_000;
/// How long the daemon waits for that answer before `timeout`.
pub const SCREENSHOT_WAIT: Duration = Duration::from_secs(35);
/// The largest serialized `tools/call` result the agent channel sends. The
/// relay is a Cloudflare Durable Object WebSocket whose per-message cap is
/// about 1 MiB and which drops the socket, silently, past it; the portal keeps
/// the same margin (`MAX_FRAME_BYTES = 900 * 1024`,
/// `nevoflux-portal/src/lib/chat/limits.ts`). The JSON-RPC and envelope
/// wrapping plus the seal add well under 1 KiB.
pub const MCP_RESULT_BUDGET: usize = 900 * 1024;
