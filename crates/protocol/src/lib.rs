// crates/protocol/src/lib.rs

//! NevoFlux Protocol - IPC message definitions for Agent communication.

/// Protocol version
pub const PROTOCOL_VERSION: &str = "5.0.0";

/// Get the protocol version
pub fn get_protocol_version() -> &'static str {
    PROTOCOL_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_version() {
        assert_eq!(get_protocol_version(), "5.0.0");
    }
}
