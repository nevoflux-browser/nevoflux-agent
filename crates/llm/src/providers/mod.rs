//! LLM Provider implementations.
//!
//! Custom providers for LLM services not natively supported by rig.

use std::pin::Pin;
use std::task::{Context, Poll};

pub mod claude_code;
pub mod gemini_cli;
pub mod kimi_agent;
pub mod qwen;

/// A stream wrapper that holds a child process and kills it on drop.
///
/// When the stream consumer is dropped (e.g. because the agent was interrupted),
/// this ensures the CLI subprocess is terminated rather than left orphaned.
pub(crate) struct ChildGuardStream<S> {
    inner: Pin<Box<S>>,
    child: Option<tokio::process::Child>,
}

impl<S> ChildGuardStream<S> {
    pub(crate) fn new(inner: S, child: tokio::process::Child) -> Self {
        Self {
            inner: Box::pin(inner),
            child: Some(child),
        }
    }
}

impl<S> futures::Stream for ChildGuardStream<S>
where
    S: futures::Stream,
{
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl<S> Drop for ChildGuardStream<S> {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // start_kill is non-blocking and safe to call from drop
            let _ = child.start_kill();
        }
    }
}
