//! The install record. Written at install, read+reversed at uninstall.

use std::path::PathBuf;

use semver::Version;
use serde::{Deserialize, Serialize};

pub const RECEIPT_VERSION: &str = "1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PathsSource {
    Daemon,
    Derived,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileReceipt {
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Receipt {
    pub receipt_version: String,
    pub protocol: String,
    pub pack: String,
    pub namespace: String,
    pub version: Version,
    pub installed_at: String,
    pub nevoflux_version: Version,
    pub paths_source: PathsSource,
    #[serde(default)]
    pub files: Vec<FileReceipt>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub seeded_pages: Vec<String>,
    #[serde(default)]
    pub imported_sources: Vec<String>,
}

impl Receipt {
    /// Compute the sha256 of file bytes as lowercase hex (used for FileReceipt).
    pub fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let r = Receipt {
            receipt_version: RECEIPT_VERSION.into(),
            protocol: "pack-protocol/0.1".into(),
            pack: "demo".into(),
            namespace: "demo".into(),
            version: Version::new(0, 1, 0),
            installed_at: "2026-06-09T00:00:00Z".into(),
            nevoflux_version: Version::new(0, 3, 0),
            paths_source: PathsSource::Daemon,
            files: vec![FileReceipt { path: "/cfg/skills/a.md".into(), sha256: "ab".into() }],
            artifacts: vec!["demo-dashboard".into()],
            seeded_pages: vec!["demo/cv".into()],
            imported_sources: vec!["demo".into()],
        };
        let json = serde_json::to_string_pretty(&r).unwrap();
        let back: Receipt = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn sha256_is_stable_lowercase_hex() {
        assert_eq!(
            Receipt::sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
