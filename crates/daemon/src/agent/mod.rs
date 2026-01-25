//! Agent execution module.

pub mod abi;
pub mod runner;
pub mod streaming;
pub mod tools;

pub use abi::{
    AgentContent, AgentProcessInput, AgentProcessOutput, AgentResult, HistoryEntry,
    PendingToolCall, ToolResult, ABI_VERSION, ABI_VERSION_FUNC, ALLOC_FUNC, ENTRY_POINT, FREE_FUNC,
    MEMORY_EXPORT,
};
pub use runner::{AgentInput, AgentMode, AgentOutput, AgentRunner, AgentRunnerConfig, ToolCall};
pub use streaming::{
    create_stream_channel, StreamEvent, StreamHandle, StreamSendError, DEFAULT_STREAM_BUFFER_SIZE,
};
pub use tools::{ToolExecutor, ToolRegistry};
