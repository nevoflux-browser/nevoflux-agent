//! The authoritative set of platform paths a pack install resolves against.

use std::path::PathBuf;

use semver::Version;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub version: Version,
    pub config_dir: PathBuf,
    pub skills_dir: PathBuf,
    pub canvas_tools_dir: PathBuf,
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
}

impl ResolvedPaths {
    /// Root for per-pack receipts and pack-private data: `{config}/packs/`.
    pub fn packs_dir(&self) -> PathBuf {
        self.config_dir.join("packs")
    }

    /// The receipt path for a given pack: `{config}/packs/<name>/receipt.json`.
    pub fn receipt_path(&self, pack: &str) -> PathBuf {
        self.packs_dir().join(pack).join("receipt.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ResolvedPaths {
        ResolvedPaths {
            version: Version::new(0, 3, 0),
            config_dir: PathBuf::from("/cfg"),
            skills_dir: PathBuf::from("/cfg/skills"),
            canvas_tools_dir: PathBuf::from("/cfg/canvas-tools"),
            config_file: PathBuf::from("/cfg/config.toml"),
            data_dir: PathBuf::from("/data"),
            db_path: PathBuf::from("/data/nevoflux.db"),
        }
    }

    #[test]
    fn packs_dir_and_receipt_path() {
        let p = sample();
        assert_eq!(p.packs_dir(), PathBuf::from("/cfg/packs"));
        assert_eq!(
            p.receipt_path("career-pack"),
            PathBuf::from("/cfg/packs/career-pack/receipt.json")
        );
    }
}
