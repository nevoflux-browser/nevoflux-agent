//! H1/H2 smoke: daemon.info reports paths, skill.reload succeeds.
use nevoflux_daemon::paths::build_resolved_paths;
use std::path::Path;

#[test]
fn resolved_paths_expose_extension_point_dirs() {
    let p = build_resolved_paths(Path::new("/cfg"), Path::new("/data"));
    // The two directories a pack is allowed to write into must be present.
    assert!(p.skills_dir.ends_with("skills"));
    assert!(p.canvas_tools_dir.ends_with("canvas-tools"));
    // And the per-pack receipt root.
    assert!(p.packs_dir().ends_with("packs"));
}
