//! Extension manifest tests.

use std::path::Path;

#[test]
fn test_extension_manifest_exists() {
    let manifest_path = Path::new("extension/manifest.json");
    assert!(manifest_path.exists(), "Extension manifest should exist");
}

#[test]
fn test_extension_manifest_valid_json() {
    let content = std::fs::read_to_string("extension/manifest.json").unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&content).unwrap();

    assert_eq!(manifest["manifest_version"], 3);
    assert!(manifest["name"].is_string());
    assert!(manifest["permissions"].is_array());
}

#[test]
fn test_native_host_template_exists() {
    let template_path = Path::new("install/native-host/com.nevoflux.agent.json.template");
    assert!(template_path.exists());
}

#[test]
fn test_extension_readme_exists() {
    let readme_path = Path::new("extension/README.md");
    assert!(readme_path.exists(), "Extension README should exist");
}

#[test]
fn test_extension_background_js_exists() {
    let bg_path = Path::new("extension/background.js");
    assert!(bg_path.exists(), "Background script should exist");
}
