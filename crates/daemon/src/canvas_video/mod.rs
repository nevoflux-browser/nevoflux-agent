//! `canvas.video.*` — composition authoring + render pipeline.
//!
//! See `docs/superpowers/specs/2026-04-19-video-skill-design.md`.

pub mod create;
pub mod ffmpeg;
pub mod frame_chunks;
pub mod job;
pub mod render;
pub mod service;

pub use service::CanvasVideoService;
