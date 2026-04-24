//! P4 integration — DESIGN-template.md parses as YAML in Rust AND
//! contains both Google base tokens + our video extension tokens.
//!
//! Run via:
//!   cargo test -p nevoflux-daemon --test design_template_parse -- --ignored --nocapture

use serde::Deserialize;
use std::collections::HashMap;
use std::fs;

#[derive(Debug, Deserialize)]
struct DesignFrontmatter {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    version: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    // Google base tokens
    #[serde(default)]
    colors: HashMap<String, String>,
    #[serde(default)]
    typography: HashMap<String, serde_yaml::Value>,
    #[serde(default)]
    spacing: HashMap<String, String>,
    #[serde(default)]
    #[allow(dead_code)]
    rounded: HashMap<String, String>,
    #[serde(default)]
    #[allow(dead_code)]
    components: HashMap<String, serde_yaml::Value>,
    // Video extension tokens
    #[serde(default)]
    motion: Option<HashMap<String, serde_yaml::Value>>,
    #[serde(default)]
    voice: Option<HashMap<String, serde_yaml::Value>>,
    #[serde(default)]
    aspect: Option<HashMap<String, serde_yaml::Value>>,
}

fn extract_frontmatter(full: &str) -> &str {
    let stripped = full
        .strip_prefix("---\n")
        .expect("document starts with ---");
    let end = stripped
        .find("\n---\n")
        .expect("frontmatter closes with ---");
    &stripped[..end]
}

#[ignore]
#[test]
fn design_template_parses_with_google_base_plus_video_extensions() {
    let path = "/ai/project/nevoflux/docs/reference/skills/video/reference/DESIGN-template.md";
    let text = fs::read_to_string(path).expect("DESIGN-template.md exists");
    let front = extract_frontmatter(&text);
    let fm: DesignFrontmatter = serde_yaml::from_str(front).expect("frontmatter parses as YAML");

    // Google base — colors must have these 4 standard keys
    assert!(!fm.name.is_empty(), "name required");
    for color in ["primary", "secondary", "background", "foreground"] {
        assert!(
            fm.colors.contains_key(color),
            "missing color token: {color} (got {:?})",
            fm.colors.keys().collect::<Vec<_>>(),
        );
    }
    assert!(
        !fm.typography.is_empty(),
        "typography must have at least one entry",
    );
    assert!(
        !fm.spacing.is_empty(),
        "spacing must have at least one entry",
    );

    // Video extensions — all three top-level extensions must be present
    let motion = fm.motion.as_ref().expect("motion extension required");
    for key in ["ease_default", "scene_duration_default", "stagger_default"] {
        assert!(motion.contains_key(key), "missing motion.{key}");
    }

    let voice = fm.voice.as_ref().expect("voice extension required");
    for key in ["provider", "voice_id", "speed"] {
        assert!(voice.contains_key(key), "missing voice.{key}");
    }

    let aspect = fm.aspect.as_ref().expect("aspect extension required");
    for key in ["default", "width", "height", "safe_zones"] {
        assert!(aspect.contains_key(key), "missing aspect.{key}");
    }
}
