//! Jev decision oracle (design v1.6 §5).

pub mod client;
pub mod economics;
pub mod export;
pub mod history;
pub mod oracle;
pub mod privacy;
pub mod render;
pub mod rpc;
pub mod signals;
#[cfg(test)]
pub(crate) mod test_support;
pub mod visibility;
pub mod wire;
