//! `manifest.json` (`nbrain/1`) — describes the bundle contents.

use serde::{Deserialize, Serialize};

use nevoflux_brain::BrainError;

pub const FORMAT: &str = "nbrain/1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sender {
    pub fingerprint: Option<String>,
    pub display_name: String,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StripRulesApplied {
    pub compiled_only: bool,
    pub frontmatter_whitelist: Vec<String>,
    pub frontmatter_redacted: Vec<String>,
    pub raw_excluded: bool,
    pub directories_excluded: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub format: String,
    pub created_at: String,
    pub sender: Sender,
    pub files: Vec<FileEntry>,
    pub strip_rules_applied: StripRulesApplied,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub expires_at: Option<String>,
}

impl Manifest {
    /// Serialize to pretty JSON bytes.
    pub fn to_json(&self) -> Result<Vec<u8>, BrainError> {
        serde_json::to_vec_pretty(self)
            .map_err(|e| BrainError::Backend(format!("manifest serialize: {e}")))
    }

    /// Parse + validate the `format` field.
    pub fn from_json(bytes: &[u8]) -> Result<Self, BrainError> {
        let m: Manifest = serde_json::from_slice(bytes)
            .map_err(|e| BrainError::MalformedArchive(format!("manifest parse: {e}")))?;
        if m.format != FORMAT {
            return Err(BrainError::UnsupportedFormat(format!(
                "manifest format {:?}, expected {FORMAT}",
                m.format
            )));
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            format: FORMAT.into(),
            created_at: "2026-05-30T00:00:00Z".into(),
            sender: Sender {
                fingerprint: None,
                display_name: "Alice".into(),
                signature: None,
            },
            files: vec![FileEntry {
                path: "concepts/yc.md".into(),
                sha256: "ab".into(),
                bytes: 12,
            }],
            strip_rules_applied: StripRulesApplied {
                compiled_only: true,
                frontmatter_whitelist: vec!["title".into()],
                frontmatter_redacted: vec!["score".into()],
                raw_excluded: true,
                directories_excluded: vec![".raw".into()],
            },
            title: "Notes".into(),
            description: "desc".into(),
            tags: vec!["rag".into()],
            expires_at: None,
        }
    }

    #[test]
    fn json_roundtrip() {
        let m = sample();
        let bytes = m.to_json().unwrap();
        assert_eq!(Manifest::from_json(&bytes).unwrap(), m);
    }

    #[test]
    fn wrong_format_rejected() {
        let mut m = sample();
        m.format = "nbrain/999".into();
        let bytes = serde_json::to_vec(&m).unwrap();
        assert!(matches!(
            Manifest::from_json(&bytes).unwrap_err(),
            BrainError::UnsupportedFormat(_)
        ));
    }

    #[test]
    fn garbage_rejected() {
        assert!(matches!(
            Manifest::from_json(b"not json").unwrap_err(),
            BrainError::MalformedArchive(_)
        ));
    }
}
