//! NevoFlux Pack protocol engine — pure logic (no daemon/tokio deps).
//! Platform side effects go through [`host::PackHost`]. Module declarations
//! are added by their respective tasks (B2–B8).

pub mod error;

pub use error::{PackError, PackResult};
