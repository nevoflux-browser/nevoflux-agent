//! Jev decision oracle (design v1.6 §5).

pub mod client;
pub mod oracle;
pub mod privacy;
#[cfg(test)]
pub(crate) mod test_support;
pub mod wire;
