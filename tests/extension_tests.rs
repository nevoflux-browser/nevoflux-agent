//! Extension manifest tests.

use std::path::PathBuf;

/// Get the project root directory.
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn test_extension_manifest_exists() {
    let manifest_path = project_root().join("extension/manifest.json");
    assert!(
        manifest_path.exists(),
        "Extension manifest should exist at {:?}",
        manifest_path
    );
}

#[test]
fn test_extension_manifest_valid_json() {
    let manifest_path = project_root().join("extension/manifest.json");
    let content =
        std::fs::read_to_string(&manifest_path).expect("Failed to read extension manifest");
    let manifest: serde_json::Value =
        serde_json::from_str(&content).expect("Failed to parse manifest as JSON");

    assert_eq!(manifest["manifest_version"], 3);
    assert!(manifest["name"].is_string());
    assert!(manifest["permissions"].is_array());
}

/// The browser registers the native host itself (NevofluxNativeHostRegistrar),
/// so what ships here are the scripts that copy the agent into it.
#[test]
fn test_native_host_update_scripts_exist() {
    for script in ["update-agent.sh", "update-agent.ps1"] {
        let path = project_root().join("install/native-host").join(script);
        assert!(path.exists(), "{script} should exist at {:?}", path);
    }
}

#[test]
fn test_extension_readme_exists() {
    let readme_path = project_root().join("extension/README.md");
    assert!(
        readme_path.exists(),
        "Extension README should exist at {:?}",
        readme_path
    );
}

#[test]
fn test_extension_background_js_exists() {
    let bg_path = project_root().join("extension/background.js");
    assert!(
        bg_path.exists(),
        "Background script should exist at {:?}",
        bg_path
    );
}
