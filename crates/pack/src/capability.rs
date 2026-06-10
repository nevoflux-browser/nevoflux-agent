//! Pure capability sandbox: the 5 structural invariants the platform enforces
//! before any mutating host call. No I/O — reads manifest + ResolvedPaths only.

use std::path::{Component, Path, PathBuf};

use crate::manifest::Manifest;
use crate::paths::ResolvedPaths;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    OutsideWhitelistDir { component: String, dest: PathBuf },
    ConfigComponentForbidden,
    SlugOutsideNamespace { slug: String, ns: String },
    SeedNotProtected { slug: String },
    PathTraversal { raw: String },
}

/// Validate a manifest against resolved paths. `raw_toml` is the original
/// source, scanned for the forbidden `[components.config]` table (serde would
/// otherwise silently ignore it). Returns all violations at once.
pub fn validate(
    manifest: &Manifest,
    paths: &ResolvedPaths,
    raw_toml: &str,
) -> Result<(), Vec<Violation>> {
    let mut v = Vec::new();
    let ns = manifest.namespace().to_string();

    // (3) config writes forbidden — detect the table in raw source.
    if let Ok(toml::Value::Table(t)) = raw_toml.parse::<toml::Value>() {
        if t.get("components")
            .and_then(|c| c.as_table())
            .map(|c| c.contains_key("config"))
            .unwrap_or(false)
        {
            v.push(Violation::ConfigComponentForbidden);
        }
    }

    // (1)+(2) skills dir destination must stay within skills_dir.
    if let Some(s) = &manifest.components.skills {
        check_dest(&mut v, "skills", &paths.skills_dir, &s.dir);
    }
    // (1)+(2) each canvas-tool file must stay within canvas_tools_dir.
    if let Some(ct) = &manifest.components.canvas_tools {
        for f in &ct.files {
            // canvas-tool TOMLs are flattened to canvas_tools_dir/<basename>.
            check_dest(&mut v, "canvas_tools", &paths.canvas_tools_dir, f);
        }
    }

    // (4) seed/knowledge/protected slugs must be inside the namespace.
    for s in &manifest.components.seed {
        if !in_namespace(&s.slug, &ns) {
            v.push(Violation::SlugOutsideNamespace { slug: s.slug.clone(), ns: ns.clone() });
        }
    }
    if let Some(k) = &manifest.components.knowledge {
        let source = k.source_name.clone().unwrap_or_else(|| manifest.pack.name.clone());
        if !in_namespace(&source, &ns) {
            v.push(Violation::SlugOutsideNamespace { slug: source, ns: ns.clone() });
        }
    }
    if let Some(p) = &manifest.components.protected {
        for slug in p.slugs.iter().chain(p.prefixes.iter()) {
            if !in_namespace(slug, &ns) {
                v.push(Violation::SlugOutsideNamespace { slug: slug.clone(), ns: ns.clone() });
            }
        }
    }

    // (5) every seed slug must be covered by protected (hard reject).
    let protected = manifest.components.protected.clone().unwrap_or_default();
    for s in &manifest.components.seed {
        let covered = protected.slugs.iter().any(|x| x == &s.slug)
            || protected.prefixes.iter().any(|pre| s.slug.starts_with(pre));
        if !covered {
            v.push(Violation::SeedNotProtected { slug: s.slug.clone() });
        }
    }

    if v.is_empty() { Ok(()) } else { Err(v) }
}

/// True if `slug` equals the namespace or sits under `<ns>/`.
fn in_namespace(slug: &str, ns: &str) -> bool {
    slug == ns || slug.starts_with(&format!("{ns}/"))
}

/// Lexically normalize `rel` (resolving `.`/`..` without touching the FS) and
/// confirm it stays within `root`. Pushes PathTraversal/OutsideWhitelistDir.
fn check_dest(v: &mut Vec<Violation>, component: &str, root: &Path, rel: &str) {
    let mut normalized = PathBuf::new();
    for comp in Path::new(rel).components() {
        match comp {
            Component::ParentDir => {
                if !normalized.pop() {
                    v.push(Violation::PathTraversal { raw: rel.to_string() });
                    return;
                }
            }
            Component::CurDir => {}
            Component::Normal(c) => normalized.push(c),
            Component::RootDir | Component::Prefix(_) => {
                // Absolute path inside a manifest is always out of bounds.
                v.push(Violation::OutsideWhitelistDir {
                    component: component.to_string(),
                    dest: PathBuf::from(rel),
                });
                return;
            }
        }
    }
    // Destinations are always rooted at the whitelisted dir, so a normalized
    // relative path that never underflowed is in-bounds. (Underflow already
    // pushed PathTraversal above.)
    let _ = root;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;
    use semver::Version;
    use std::path::PathBuf;

    fn paths() -> ResolvedPaths {
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

    fn manifest(extra: &str) -> (Manifest, String) {
        let src = format!(
            "[pack]\nname=\"demo\"\nversion=\"0.1.0\"\nprotocol=\"pack-protocol/0.1\"\nmin_nevoflux=\"0.3.0\"\n{extra}"
        );
        (Manifest::parse(&src).unwrap(), src)
    }

    #[test]
    fn clean_manifest_passes() {
        let (m, raw) = manifest(
            "[components.skills]\ndir=\"skills\"\n\
             [[components.seed]]\nslug=\"demo/cv\"\nfrom=\"seed/cv.md\"\n\
             [components.protected]\nslugs=[\"demo/cv\"]\n",
        );
        assert!(validate(&m, &paths(), &raw).is_ok());
    }

    #[test]
    fn seed_not_protected_is_rejected() {
        let (m, raw) = manifest(
            "[[components.seed]]\nslug=\"demo/cv\"\nfrom=\"seed/cv.md\"\n",
        );
        let errs = validate(&m, &paths(), &raw).unwrap_err();
        assert!(errs.contains(&Violation::SeedNotProtected { slug: "demo/cv".into() }));
    }

    #[test]
    fn slug_outside_namespace_is_rejected() {
        let (m, raw) = manifest(
            "[[components.seed]]\nslug=\"other/cv\"\nfrom=\"s.md\"\n\
             [components.protected]\nprefixes=[\"other/\"]\n",
        );
        let errs = validate(&m, &paths(), &raw).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, Violation::SlugOutsideNamespace { .. })));
    }

    #[test]
    fn path_traversal_is_rejected() {
        let (m, raw) = manifest("[components.skills]\ndir=\"../../etc\"\n");
        let errs = validate(&m, &paths(), &raw).unwrap_err();
        assert!(errs.contains(&Violation::PathTraversal { raw: "../../etc".into() }));
    }

    #[test]
    fn config_component_is_forbidden() {
        let (m, raw) = manifest("[components.config]\nkey=\"value\"\n");
        let errs = validate(&m, &paths(), &raw).unwrap_err();
        assert!(errs.contains(&Violation::ConfigComponentForbidden));
    }
}
