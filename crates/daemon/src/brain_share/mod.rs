//! `.nbrain` encrypted knowledge-base sharing — local core (M5-A).
//!
//! Pure logic only (no gbrain coupling): binary envelope, crypto, manifest,
//! strip pipeline, tar+zstd packing, and the `seal`/`open` orchestration.

pub mod crypto;
pub mod manifest;
pub mod nbrain_format;
