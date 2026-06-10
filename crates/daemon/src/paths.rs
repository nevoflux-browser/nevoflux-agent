//! H1: aggregate the daemon's scattered path knowledge into one
//! `ResolvedPaths` value, the single authority every pack install resolves
//! against (kills path drift).

use std::path::{Path, PathBuf};

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

/// Resolve the authoritative paths the way the daemon actually resolves them:
/// config dir from `AgentConfig::default_config_path()`'s parent (matching how
/// config is loaded, incl. the macOS XDG fallback), data dir from the same
/// logic `main.rs::get_data_dir` uses.
///
/// This is the single source of truth both `daemon.info` and the pack handlers
/// resolve against, killing path drift between the two.
pub fn resolve_from_daemon() -> ResolvedPaths {
    let config_dir = crate::config::AgentConfig::default_config_path()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let data_dir = std::env::var_os("NEVOFLUX_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            directories::ProjectDirs::from("com", "nevoflux", "nevoflux")
                .map(|d| d.data_dir().to_path_buf())
        })
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".nevoflux")
        });
    build_resolved_paths(&config_dir, &data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

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
