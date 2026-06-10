//! H1: aggregate the daemon's scattered path knowledge into one
//! `ResolvedPaths` value, the single authority every pack install resolves
//! against (kills path drift).

use std::path::Path;

use nevoflux_pack::ResolvedPaths;

/// Build the authoritative resolved paths from the daemon's config and data
/// directories.
///
/// - `config_dir`: e.g. `~/.config/nevoflux` (see `config.rs`)
/// - `data_dir`:   e.g. `~/.local/share/nevoflux` (see `main.rs::get_data_dir`)
pub fn build_resolved_paths(config_dir: &Path, data_dir: &Path) -> ResolvedPaths {
    ResolvedPaths {
        version: semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .unwrap_or_else(|_| semver::Version::new(0, 0, 0)),
        config_dir: config_dir.to_path_buf(),
        skills_dir: config_dir.join("skills"),
        canvas_tools_dir: config_dir.join("canvas-tools"),
        config_file: config_dir.join("config.toml"),
        data_dir: data_dir.to_path_buf(),
        db_path: data_dir.join("nevoflux.db"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn derives_subpaths_from_config_and_data_dirs() {
        let p = build_resolved_paths(Path::new("/cfg"), Path::new("/data"));
        assert_eq!(p.skills_dir, PathBuf::from("/cfg/skills"));
        assert_eq!(p.canvas_tools_dir, PathBuf::from("/cfg/canvas-tools"));
        assert_eq!(p.config_file, PathBuf::from("/cfg/config.toml"));
        assert_eq!(p.db_path, PathBuf::from("/data/nevoflux.db"));
        assert_eq!(p.packs_dir(), PathBuf::from("/cfg/packs"));
    }
}
