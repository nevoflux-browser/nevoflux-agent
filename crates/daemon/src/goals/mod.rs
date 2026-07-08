//! Goals subsystem — session-scoped success conditions with an LLM evaluator.
//!
//! A *goal* is a natural-language success condition attached to a chat session
//! (e.g. "the report has been posted to #general"). After every chat turn the
//! [`manager::GoalManager::after_turn`] hook asks a zero-tool LLM *evaluator*
//! whether the condition is met yet, given the tail of the conversation. When
//! it is not met and the turn budget is not exhausted, `after_turn` hands the
//! caller a synthetic `<GOAL-CONTINUATION>` directive to drive another turn;
//! when it is met (or the budget runs out, or the evaluator is broken) it
//! returns `None` and the loop stops.
//!
//! Task 2.1 landed the storage layer (`GoalRepository`, `GoalRecord`). Task 2.2
//! added:
//! - [`evaluator`]: `resolve_evaluator` (pick a direct-API provider/model/key,
//!   rejecting ACP providers), the verbatim evaluator system prompt, the pure
//!   `parse_verdict` / `clip_transcript` cores, and the async `evaluate` call.
//! - [`events`]: the `system:goal:*` EventBus surface (mirrors
//!   `schedules::events`).
//! - [`manager`]: `GoalManager` (`set` / `status` / `clear` / `after_turn`) and
//!   the pure `apply_verdict` decision core.
//!
//! Task 2.3 adds [`tools`] (the `goal_*` LLM-callable tool dispatcher).

pub mod evaluator;
pub mod events;
pub mod manager;
pub mod tools;

pub use evaluator::{evaluate, resolve_evaluator, EvaluatorChoice, Verdict};
pub use events::GoalEvents;
pub use manager::GoalManager;
pub use tools::execute_goal_tool;
