//! Jev decision oracle (design v1.6 §5).

pub mod client;
pub mod economics;
pub mod export;
pub mod history;
pub mod oracle;
pub mod privacy;
pub mod rebuild;
pub mod render;
pub mod rpc;
pub mod signals;
pub mod skills;
#[cfg(test)]
pub(crate) mod test_support;
pub mod tools;
pub mod turn;
pub mod visibility;
pub mod wire;
