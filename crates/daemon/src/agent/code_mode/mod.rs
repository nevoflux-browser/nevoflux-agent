//! Agent Code Mode - Executes LLM-generated Python via Pydantic Monty.
//!
//! Four-layer constraint system:
//! 1. Prompt constraint (prevention) - system prompt with allow-list
//! 2. Auto-fix (mechanical transform) - strip imports, decorators
//! 3. Linter (detection) - regex-based unsupported construct detection
//! 4. Runtime + smart retry (recovery) - Monty execution with error repair

pub mod auto_fixer;
pub mod executor;
pub mod linter;
pub mod repair_prompt;

pub use executor::{CodeModeExecutor, CodeModeResult, ToolCallResult};
